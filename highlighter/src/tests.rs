use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use indexmap::{IndexMap, IndexSet};
use skidder::Repo;
use std::cell::OnceCell;
use std::sync::LazyLock;
use tree_sitter::{Grammar, InputEdit, Point};

use crate::config::{LanguageConfig, LanguageLoader};
use crate::fixtures::{check_highlighter_fixture, check_injection_fixture};
use crate::highlighter::{Highlight, HighlightEvent, Highlighter};
use crate::injections_query::InjectionLanguageMarker;
use crate::{Language, Layer, LayerData, Syntax};

const PARSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

static GRAMMARS: LazyLock<Vec<PathBuf>> = LazyLock::new(|| {
    let skidder_config = skidder_config();
    skidder::fetch(&skidder_config, false).unwrap();
    skidder::build_all_grammars(&skidder_config, false, None).unwrap();
    let grammars = skidder::list_grammars(&skidder_config).unwrap();
    assert!(!grammars.is_empty());
    grammars
});

fn skidder_config() -> skidder::Config {
    skidder::Config {
        repos: vec![Repo::Local {
            // `./test-grammars` in the root of the repo.
            path: Path::new("../test-grammars").canonicalize().unwrap(),
        }],
        index: PathBuf::new(),
        verbose: true,
    }
}

#[derive(Debug, Clone, Default)]
struct Overwrites {
    highlights: Option<String>,
    locals: Option<String>,
    injections: Option<String>,
}

fn get_grammar(lang_name: &str, overwrites: &Overwrites) -> LanguageConfig {
    let skidder_config = skidder_config();
    let grammar_dir = skidder_config.grammar_dir(lang_name).unwrap();
    let parser_path = skidder::build_grammar(&skidder_config, lang_name, false).unwrap();
    let grammar = unsafe { Grammar::new(lang_name, &parser_path).unwrap() };
    let highlights_query_path = grammar_dir.join("highlights.scm");
    let injections_query_path = grammar_dir.join("injections.scm");
    if !injections_query_path.exists() {
        println!("\x1b[36mskipping loading of injections for {lang_name:?} since {injections_query_path:?} does not exist\x1b[0m");
    }
    let locals_query_path = grammar_dir.join("locals.scm");
    if !locals_query_path.exists() {
        println!("\x1b[36mskipping loading of locals for {lang_name:?} since {locals_query_path:?} does not exist\x1b[0m");
    }
    LanguageConfig::new(
        grammar,
        &overwrites.highlights.clone().unwrap_or_else(|| {
            fs::read_to_string(&highlights_query_path)
                .map_err(|err| {
                    format!(
                        "failed to read highlights in {}: {err}",
                        highlights_query_path.display()
                    )
                })
                .unwrap()
        }),
        &overwrites
            .injections
            .clone()
            .unwrap_or_else(|| fs::read_to_string(&injections_query_path).unwrap_or_default()),
        &overwrites
            .locals
            .clone()
            .unwrap_or_else(|| fs::read_to_string(&locals_query_path).unwrap_or_default()),
    )
    .unwrap()
}

#[derive(Debug)]
struct TestLanguageLoader {
    languages: IndexMap<String, Language>,
    lang_config: Box<[OnceCell<LanguageConfig>]>,
    overwrites: Box<[Overwrites]>,
    test_theme: RefCell<IndexSet<String>>,
    /// If set, only scopes in this set receive a highlight; others return `None`.
    highlight_filter: Option<HashSet<String>>,
}

impl TestLanguageLoader {
    fn new() -> Self {
        let grammars = &GRAMMARS;

        Self {
            lang_config: (0..grammars.len()).map(|_| OnceCell::new()).collect(),
            overwrites: vec![Overwrites::default(); grammars.len()].into_boxed_slice(),
            test_theme: RefCell::default(),
            highlight_filter: None,
            languages: grammars
                .iter()
                .enumerate()
                .map(|(i, grammar)| {
                    (
                        grammar.file_name().unwrap().to_str().unwrap().to_owned(),
                        Language::new(i as u32),
                    )
                })
                .collect(),
        }
    }

    fn get(&self, name: &str) -> Language {
        self.languages[name]
    }

    fn overwrite_injections(&mut self, lang: &str, content: String) {
        let lang = self.get(lang);
        self.overwrites[lang.idx()].injections = Some(content);
        self.lang_config[lang.idx()] = OnceCell::new();
    }

    fn overwrite_highlights(&mut self, lang: &str, content: String) {
        let lang = self.get(lang);
        self.overwrites[lang.idx()].highlights = Some(content);
        self.lang_config[lang.idx()] = OnceCell::new();
    }

    fn shadow_injections(&mut self, lang: &str, content: &str) {
        let lang = self.get(lang);
        let skidder_config = skidder_config();
        let grammar = self.languages.get_index(lang.idx()).unwrap().0;
        let grammar_dir = skidder_config.grammar_dir(grammar).unwrap();
        let mut injections =
            fs::read_to_string(grammar_dir.join("injections.scm")).unwrap_or_default();
        injections.push('\n');
        injections.push_str(content);
        self.overwrites[lang.idx()].injections = Some(injections);
        self.lang_config[lang.idx()] = OnceCell::new();
    }

    /// Restrict the test theme so only the given scopes receive a highlight index.
    /// All other scopes will return `None` from the configure callback.
    fn set_highlight_filter(&mut self, scopes: impl IntoIterator<Item = impl Into<String>>) {
        self.highlight_filter = Some(scopes.into_iter().map(Into::into).collect());
    }

    fn shadow_highlights(&mut self, lang: &str, content: &str) {
        let lang = self.get(lang);
        let skidder_config = skidder_config();
        let grammar = self.languages.get_index(lang.idx()).unwrap().0;
        let grammar_dir = skidder_config.grammar_dir(grammar).unwrap();
        let mut highlights = fs::read_to_string(grammar_dir.join("highlights.scm")).unwrap();
        highlights.push('\n');
        highlights.push_str(content);
        self.overwrites[lang.idx()].highlights = Some(highlights);
        self.lang_config[lang.idx()] = OnceCell::new();
    }
}

impl LanguageLoader for TestLanguageLoader {
    fn language_for_marker(&self, marker: InjectionLanguageMarker) -> Option<Language> {
        match marker {
            InjectionLanguageMarker::Name(name) => self.languages.get(name).copied(),
            InjectionLanguageMarker::Match(text) => {
                let name: Cow<str> = text.into();
                self.languages.get(name.as_ref()).copied()
            }
            _ => None,
        }
    }

    fn get_config(&self, lang: Language) -> Option<&LanguageConfig> {
        let config = self.lang_config[lang.idx()].get_or_init(|| {
            let config = get_grammar(
                self.languages.get_index(lang.idx()).unwrap().0,
                &self.overwrites[lang.idx()],
            );
            let mut theme = self.test_theme.borrow_mut();
            config.configure(|scope| {
                if let Some(ref allowed) = self.highlight_filter {
                    if !allowed.contains(scope) {
                        return None;
                    }
                }
                Some(Highlight::new(theme.insert_full(scope.to_owned()).0 as u32))
            });
            config
        });
        Some(config)
    }
}

fn lang_for_path(path: &Path, loader: &TestLanguageLoader) -> Language {
    match path
        .extension()
        .and_then(|it| it.to_str())
        .unwrap_or_default()
    {
        "rs" => loader.get("rust"),
        "html" => loader.get("html"),
        "css" => loader.get("css"),
        "erl" => loader.get("erlang"),
        "md" => loader.get("markdown"),
        extension => panic!("unknown file type .{extension}"),
    }
}

fn highlight_fixture(loader: &TestLanguageLoader, fixture: impl AsRef<Path>) {
    let path = Path::new("../fixtures").join(fixture);
    let lang = lang_for_path(&path, loader);
    check_highlighter_fixture(
        path,
        "// ",
        lang,
        loader,
        |highlight| loader.test_theme.borrow()[highlight.idx()].clone(),
        |_| ..,
    )
}

fn injection_fixture(loader: &TestLanguageLoader, fixture: impl AsRef<Path>) {
    let path = Path::new("../fixtures").join(fixture);
    let lang = lang_for_path(&path, loader);
    check_injection_fixture(
        path,
        "// ",
        lang,
        loader,
        |lang| loader.languages.get_index(lang.idx()).unwrap().0.clone(),
        |_| ..,
    )
}

#[test]
fn highlight() {
    let loader = TestLanguageLoader::new();
    highlight_fixture(&loader, "highlighter/hello_world.rs");
}

#[test]
fn layers() {
    let loader = TestLanguageLoader::new();

    let input = "/// Says hello.
///
/// this is *markdown-inline* markdown
///
/// # Example
///
/// ```rust
/// fn add(left: usize, right: usize) -> usize {
///     left + right
/// }
/// ```
pub fn hello() {}";

    let syntax = Syntax::new(input.into(), loader.get("rust"), PARSE_TIMEOUT, &loader).unwrap();

    let assert_injection = |snippet: &str, expected: &[&str]| {
        assert!(!expected.is_empty(), "all layers have at least 1 injection");

        let layer_lang_name = |layer: Layer| {
            loader
                .languages
                .get_index(syntax.layer(layer).language.idx())
                .unwrap()
                .0
                .clone()
        };

        let snippet_start = input.find(snippet).unwrap() as u32;
        let snippet_end = snippet_start + snippet.len() as u32;

        let layers = syntax
            .layers_for_byte_range(snippet_start, snippet_end)
            .map(layer_lang_name)
            .collect::<Vec<_>>();

        assert_eq!(&layers, expected, r#"snippet: "{snippet}""#);

        let layer = syntax.layer_for_byte_range(snippet_start, snippet_end);
        assert_eq!(
            &layer_lang_name(layer),
            expected.last().unwrap(),
            "last layer is the smallest fully encompassing layer"
        );
    };

    // Rust function in a code block in the rust documentation
    assert_injection("fn add(left: usize, ri", &["rust", "markdown", "rust"]);

    // Markdown heading  `# Example`
    assert_injection("# Example", &["rust", "markdown"]);

    // Outer-most Rust function `hello`
    assert_injection("pub fn hello() {}", &["rust"]);

    // Paragraph in the rust documentation
    assert_injection("markdown-inline", &["rust", "markdown", "markdown-inline"]);
}

#[test]
fn layers_for_byte_range_injection_boundary() {
    let mut loader = TestLanguageLoader::new();
    // Inject into each block comment as a separate (non-combined) markdown injection.
    loader.overwrite_injections(
        "rust",
        r#"((block_comment) @injection.content
 (#set! injection.language "markdown")
 (#set! injection.include-children))"#
            .to_string(),
    );
    // Two adjacent block comments share a boundary at the byte where the first ends
    // and the second begins. The boundary byte belongs to the second injection.
    let input = "/* first *//* second */";
    let syntax = Syntax::new(input.into(), loader.get("rust"), PARSE_TIMEOUT, &loader).unwrap();

    let second_start = input.find("/* second */").unwrap() as u32;

    let root = syntax.layer(syntax.root());
    // The first injection covers 0..second_start; the second starts at second_start.
    let first_inj_layer = root.injection_at_byte_idx(second_start - 1).unwrap().layer;
    let second_inj_layer = root.injection_at_byte_idx(second_start).unwrap().layer;

    let layers: Vec<_> = syntax
        .layers_for_byte_range(second_start, second_start)
        .collect();
    // Querying at the boundary byte reaches the second injection's layer, not the first.
    assert!(layers.contains(&second_inj_layer));
    assert!(!layers.contains(&first_inj_layer));
}

#[test]
fn highlight_overlaps_with_injection() {
    let loader = TestLanguageLoader::new();
    // The comment node is highlighted both by the comment capture and as an injection for the
    // comment grammar.
    highlight_fixture(&loader, "highlighter/comment.html");
}

#[test]
fn rust_parameter_locals() {
    let loader = TestLanguageLoader::new();
    highlight_fixture(&loader, "highlighter/rust_parameter_locals.rs");
}

#[test]
fn codefence_rust_doc_comments() {
    let loader = TestLanguageLoader::new();
    highlight_fixture(&loader, "highlighter/codefence_rust_doc_comments.md");
}

#[test]
fn parameters_within_injections_within_injections() {
    let loader = TestLanguageLoader::new();
    // The root language is Rust. Then markdown is injected in a doc comment. Then within that
    // we have a code fence which is Rust again. Within that block we check that locals are
    // highlighted as expected.
    highlight_fixture(&loader, "highlighter/injectionception.rs");
    injection_fixture(&loader, "injections/injectionception.rs");
}

#[test]
fn html_in_edoc_in_erlang() {
    let loader = TestLanguageLoader::new();
    // This fixture exhibited a bug (which has been fixed) where a combined injection became
    // dormant at the same time as a new highlight started, causing a total reset of all
    // highlights (incorrectly).
    highlight_fixture(&loader, "highlighter/html_in_edoc_in_erlang.erl");
    injection_fixture(&loader, "injections/html_in_edoc_in_erlang.erl");
}

#[test]
fn non_local_pattern() {
    let mut loader = TestLanguageLoader::new();
    // Pretend that `this` is a builtin like `self`, but only when it is not a parameter.
    loader.shadow_highlights(
        "rust",
        r#"
((identifier) @variable.builtin
 (#eq? @variable.builtin "this")
 (#is-not? local))
"#,
    );
    highlight_fixture(&loader, "highlighter/non_local.rs");
}

#[test]
fn reference_highlight_starts_after_definition_ends() {
    let loader = TestLanguageLoader::new();
    // In this example the function name matches one of the parameters. The function name can be
    // a reference but since the definition occurs after the function name it, the function name
    // should not be highlighted as a parameter.
    highlight_fixture(
        &loader,
        "highlighter/reference_highlight_starts_after_definition_ends.rs",
    );
}

#[test]
fn combined_injection() {
    let mut loader = TestLanguageLoader::new();
    loader.shadow_injections(
        "rust",
        r#"
((doc_comment) @injection.content
 (#set! injection.language "markdown")
 (#set! injection.combined))"#,
    );
    highlight_fixture(&loader, "highlighter/rust_doc_comment.rs");
}

#[test]
fn injection_in_child() {
    let mut loader = TestLanguageLoader::new();
    // here doc_comment is a child of line_comment which has higher precedence
    // however since it doesn't include children the doc_comment injection is
    // still active here. This could probably use a more real world use case (maybe nix?)
    loader.shadow_injections(
        "rust",
        r#"
([(line_comment) (block_comment)] @injection.content
 (#set! injection.language "comment"))

([(line_comment (doc_comment) @injection.content) (block_comment (doc_comment) @injection.content)]
 (#set! injection.language "markdown")
 (#set! injection.combined))
"#,
    );
    highlight_fixture(&loader, "highlighter/rust_doc_comment.rs");
    injection_fixture(&loader, "injections/rust_doc_comment.rs");
}

#[test]
fn injection_precedence() {
    let mut loader = TestLanguageLoader::new();
    loader.shadow_injections(
        "rust",
        r#"
([(line_comment) (block_comment)] @injection.content
 (#set! injection.language "comment")
 (#set! injection.include-children))

([(line_comment (doc_comment) @injection.content) (block_comment (doc_comment) @injection.content)]
 (#set! injection.language "markdown")
 (#set! injection.combined))
 "#,
    );
    highlight_fixture(&loader, "highlighter/rust_doc_comment.rs");

    loader.shadow_injections(
        "rust",
        r#"
([(line_comment (doc_comment) @injection.content) (block_comment (doc_comment) @injection.content)]
 (#set! injection.language "markdown")
 (#set! injection.combined))

([(line_comment) (block_comment)] @injection.content
 (#set! injection.language "comment")
 (#set! injection.include-children))
 "#,
    );
    highlight_fixture(&loader, "highlighter/rust_no_doc_comment.rs");
    injection_fixture(&loader, "injections/rust_no_doc_comment.rs");

    loader.shadow_injections(
        "rust",
        r#"
((macro_invocation
   macro:
     [
       (scoped_identifier
         name: (_) @_macro_name)
       (identifier) @_macro_name
     ]
   (token_tree . (_) . (_) . (string_literal) @injection.content))
 (#any-of? @_macro_name
  ; std
  "some_macro")
  (#set! injection.language "rust")
  (#set! injection.include-children))
    "#,
    );
    injection_fixture(&loader, "injections/overlapping_injection.rs");
}

#[test]
fn rust_nested_vec() {
    let loader = TestLanguageLoader::new();
    highlight_fixture(&loader, "highlighter/nested_vec.rs");
    injection_fixture(&loader, "injections/nested_vec.rs");
}

#[test]
fn edit_remove_and_add_injection_layer() {
    let loader = TestLanguageLoader::new();
    // Add another backtick, causing the double old backtick to become a codefence and the second
    // HTML comment to become the codefence's body.
    // When we reuse the injection for the HTML comments, we need to be sure to re-parse the HTML
    // layer so that it recognizes that the second comment is no longer valid.
    let before_text = "<!---->\n``\n<!---->";
    let after_text = "<!---->\n```\n<!---->";
    let edit = InputEdit {
        start_byte: 10,
        old_end_byte: 10,
        new_end_byte: 11,
        start_point: Point::ZERO,
        old_end_point: Point::ZERO,
        new_end_point: Point::ZERO,
    };
    let mut syntax = Syntax::new(
        before_text.into(),
        loader.get("markdown"),
        PARSE_TIMEOUT,
        &loader,
    )
    .unwrap();
    // The test here is that `Syntax::update` can apply the edit `Ok(_)` without panicking.
    syntax
        .update(after_text.into(), PARSE_TIMEOUT, &[edit], &loader)
        .unwrap();

    // Now test the inverse. Start with the after text and edit it to be the before text. In this
    // case an injection is added for the HTML comment.
    let edit = InputEdit {
        start_byte: 10,
        old_end_byte: 11,
        new_end_byte: 10,
        start_point: Point::ZERO,
        old_end_point: Point::ZERO,
        new_end_point: Point::ZERO,
    };
    let mut syntax = Syntax::new(
        after_text.into(),
        loader.get("markdown"),
        PARSE_TIMEOUT,
        &loader,
    )
    .unwrap();
    // The test here is that `Syntax::update` can apply the edit `Ok(_)` without panicking.
    syntax
        .update(before_text.into(), PARSE_TIMEOUT, &[edit], &loader)
        .unwrap();
}

#[test]
fn markdown_bold_highlight() {
    let loader = TestLanguageLoader::new();
    // This is a very simple case to check that adjacent equivalent highlights are merged
    // properly: the `punctuation.bracket` highlight on the consecutive `*`s should be combined
    // into one span.
    highlight_fixture(&loader, "highlighter/markdown_bold.md");
}

#[test]
fn css_parent_child_highlight_precedence() {
    let mut loader = TestLanguageLoader::new();
    // NOTE: the pattern being tested here `((color_value) "#") @string.special` is odd and should
    // be rewritten to `(color_value "#" @string.special)` - that was probably the original intent
    // of the pattern. We overwrite the highlights to take the parts we need for this case so that
    // if/when we fix that pattern in the future it does not break this test case.
    loader.overwrite_highlights(
        "css",
        r##"
((property_name) @variable
 (#match? @variable "^--"))

"#" @punctuation
((color_value) "#") @string.special
(color_value) @string.special

[";" ":"] @punctuation.delimiter
"##
        .to_string(),
    );

    // In this case two patterns fight over the `#` character and both should actually highlight
    // it. Because of the odd way that the pattern `((color_value) "#") @string.special` is
    // written, the QueryIter yields the captures in the opposite order it would normally:
    // first the child pattern `{Node # 9..10}` and then `{Node color_value 9..13}`.
    //
    // This case checks the invariant that "`active_highlights` ends are sorted descending" is
    // preserved.
    highlight_fixture(&loader, "highlighter/parent_child_highlight_precedence.css");
}

#[test]
fn edoc_code_combined_injection() {
    let loader = TestLanguageLoader::new();

    highlight_fixture(&loader, "highlighter/edoc_code_combined_injection.erl");
    injection_fixture(&loader, "injections/edoc_code_combined_injection.erl");
}

#[test]
fn edoc_code_combined_injection_in_markdown() {
    let loader = TestLanguageLoader::new();
    // Same as the above but within markdown to add extra layers.
    highlight_fixture(
        &loader,
        "highlighter/edoc_code_combined_injection_in_markdown.md",
    );
}

#[test]
fn combined_injection_no_invalid_ranges() {
    // Regression test for https://github.com/helix-editor/tree-house/issues/35.
    //
    // A Rust file with multiple block doc-comments and a line doc-comment in an impl block
    // used to trigger Error::InvalidRanges when computing injection ranges for the combined
    // markdown layer, because intersect_ranges_impl could produce ranges where start > end.
    let mut loader = TestLanguageLoader::new();
    loader.shadow_injections(
        "rust",
        r#"
((doc_comment) @injection.content
 (#set! injection.language "markdown")
 (#set! injection.combined))"#,
    );
    let input = "/**
 * foo
 */

impl Foo {
    /**
     * foo
     */

    /// foo
}";
    let result = Syntax::new(input.into(), loader.get("rust"), PARSE_TIMEOUT, &loader);
    assert!(
        result.is_ok(),
        "parsing should succeed without Error::InvalidRanges, got: {:?}",
        result.err()
    );
}

#[test]
fn highlighter_no_panic_overlapping_captures_missing_theme() {
    // Regression test for https://github.com/helix-editor/helix/issues/14751.
    //
    // When multiple captures match the same node and some have no highlight in the
    // current theme, `start_highlight` can remove entries that were present in
    // `active_highlights` before the current `advance()` call began. The subsequent
    // slice `active_highlights[prev_stack_size..]` then panics.
    let mut loader = TestLanguageLoader::new();
    loader.overwrite_highlights(
        "python",
        r#"
(attribute attribute: (identifier) @variable.other.member)
((identifier) @type   (#match? @type "^[A-Z]"))
(decorator) @function
((identifier) @constant  (#match? @constant "^_*[A-Z][A-Z\d_]*$"))
"#
        .to_string(),
    );
    // `@type` and `@constant` both match `B` but resolve to None in this theme.
    // Each None-returning capture removes the previously-added highlight for `B`,
    // and the third removal walks back into highlights added by a prior advance() call.
    loader.set_highlight_filter(["function", "variable.other.member"]);

    let source = "@a.B";
    let syntax = Syntax::new(source.into(), loader.get("python"), PARSE_TIMEOUT, &loader)
        .expect("parse should succeed");
    let mut highlighter = Highlighter::new(&syntax, source.into(), &loader, 0..);
    while highlighter.next_event_offset() != u32::MAX {
        highlighter.advance();
    }
}

#[test]
fn highlighter_ancestor_highlight_not_replaced_by_child_capture() {
    // Regression test for the sibling bug in start_highlight: when the sequence of
    // captures for a child node is Some/None/Some, and the child shares an end
    // byte with an ancestor highlight from a prior advance() call, the old code
    // would find the ancestor via rposition and replace it with the last Some capture.
    // The ancestor's highlight would silently vanish from active_highlights, and the
    // Push event for the child would be empty.
    //
    // The fix scopes the rposition search to highlights added in the current advance()
    // call only. The ancestor is invisible to the search, so the last Some capture is
    // pushed as a new entry alongside the ancestor.
    let mut loader = TestLanguageLoader::new();
    loader.overwrite_highlights(
        "python",
        r#"
(decorator) @function
(attribute attribute: (identifier) @variable.other.member)
((identifier) @type     (#match? @type     "^[A-Z]"))
((identifier) @constant (#match? @constant "^[A-Z]"))
"#
        .to_string(),
    );
    // For `B` in `@a.B`:
    //   @variable.other.member: Some (push)
    //   @type:                  None (remove, not in theme)
    //   @constant:              Some (old: replaces ancestor @function; new: pushes alongside)
    loader.set_highlight_filter(["function", "variable.other.member", "constant"]);

    let source = "@a.B";
    let syntax = Syntax::new(source.into(), loader.get("python"), PARSE_TIMEOUT, &loader)
        .expect("parse should succeed");
    let mut highlighter = Highlighter::new(&syntax, source.into(), &loader, 0..);

    // Advance past events before `B` (offset 3).
    let offset_of_b = source.find('B').unwrap() as u32;
    while highlighter.next_event_offset() < offset_of_b {
        highlighter.advance();
    }
    assert_eq!(highlighter.next_event_offset(), offset_of_b);

    let (event, pushed) = highlighter.advance();
    assert_eq!(event, HighlightEvent::Push);
    // The @constant highlight should be pushed as a new entry.
    assert_eq!(
        pushed.len(),
        1,
        "expected exactly one new highlight pushed for `B`"
    );
    // The ancestor @function highlight must still be present — it was not replaced.
    assert_eq!(
        highlighter.active_highlights().len(),
        2,
        "both the decorator's @function and `B`'s @constant should be active"
    );
}

#[test]
fn combined_layer_reuse_tracks_moves() {
    let loader = TestLanguageLoader::new();
    // Two html blocks around a markdown paragraph: one combined html layer
    let before = "intro\n\n<!-- a -->\n\npara\n\n<!-- b -->\n";
    let mut syntax = Syntax::new(
        before.into(),
        loader.get("markdown"),
        PARSE_TIMEOUT,
        &loader,
    )
    .unwrap();

    // Block A moves one byte to the right while block B's offset nets out to zero.
    // The reused layer's tree must still be moved.
    let after = "intro!\n\n<!-- a -->\n\npra\n\n<!-- b -->\n";
    let edits = [
        InputEdit {
            start_byte: 5,
            old_end_byte: 5,
            new_end_byte: 6,
            start_point: Point::ZERO,
            old_end_point: Point::ZERO,
            new_end_point: Point::ZERO,
        },
        InputEdit {
            start_byte: 20,
            old_end_byte: 21,
            new_end_byte: 20,
            start_point: Point::ZERO,
            old_end_point: Point::ZERO,
            new_end_point: Point::ZERO,
        },
    ];
    syntax
        .update(after.into(), PARSE_TIMEOUT, &edits, &loader)
        .unwrap();

    let html = loader.get("html");
    let layer = syntax
        .layers_for_byte_range(9, 17)
        .find(|&layer| syntax.layer(layer).language == html)
        .expect("an html layer covers the first comment");
    let root = syntax.layer(layer).tree().unwrap().root_node();
    assert_eq!(
        root.byte_range(),
        8..36,
        "the html layer's tree must span the comments' new positions"
    );
}

#[test]
fn combined_layer_reused_without_reparse() {
    let loader = TestLanguageLoader::new();
    let markdown = loader.get("markdown");
    let html = loader.get("html");
    // The edits leave blocks of the combined html layer unchanged
    let t0 = "xx\n\n<!-- a -->\n\n<!-- b -->\n\nmore text\n";
    let t1 = "xxy\n\n<!-- a -->\n\n<!-- b -->\n\nmorex text\n";
    let mut syntax = Syntax::new(t0.into(), markdown, PARSE_TIMEOUT, &loader).unwrap();
    let before = root_ids(&syntax);
    syntax
        .update(
            t1.into(),
            PARSE_TIMEOUT,
            &[edit(2, 2, 3), edit(32, 32, 33)],
            &loader,
        )
        .unwrap();
    // The layer should be reused since it didn't change
    let html_ids = |ids: Vec<(Language, usize)>| ids.into_iter().filter(|&(lang, _)| lang == html);
    assert!(
        html_ids(root_ids(&syntax)).eq(html_ids(before)),
        "the html layer was re-parsed"
    );
}

/// Each layer's root node id, which only changes when the layer is re-parsed.
fn root_ids(syntax: &Syntax) -> Vec<(Language, usize)> {
    syntax
        .layers
        .iter()
        .filter_map(|(_, layer)| {
            Some((layer.language, layer.parse_tree.as_ref()?.root_node().id()))
        })
        .collect()
}

fn edit(start_byte: u32, old_end_byte: u32, new_end_byte: u32) -> InputEdit {
    InputEdit {
        start_byte,
        old_end_byte,
        new_end_byte,
        start_point: Point::ZERO,
        old_end_point: Point::ZERO,
        new_end_point: Point::ZERO,
    }
}

/// Oracle for incremental updates: the updated syntax must have the same layers,
/// injections and highlights as a fresh parse of `text`.
#[track_caller]
fn assert_matches_fresh(loader: &TestLanguageLoader, lang: Language, syntax: &Syntax, text: &str) {
    let fresh = Syntax::new(text.into(), lang, PARSE_TIMEOUT, loader).unwrap();
    let [incremental, fresh] = [syntax, &fresh].map(|syntax| render(loader, syntax, text));
    assert_eq!(
        incremental.0, fresh.0,
        "layers diverged from a fresh parse of {text:?}"
    );
    assert_eq!(
        incremental.1, fresh.1,
        "injections diverged from a fresh parse of {text:?}"
    );
    assert_eq!(
        incremental.2, fresh.2,
        "highlights diverged from a fresh parse of {text:?}"
    );
}

/// The layers, injections and highlights of `syntax`.
fn render(
    loader: &TestLanguageLoader,
    syntax: &Syntax,
    text: &str,
) -> (Vec<String>, String, String) {
    use crate::fixtures::{highlighter_fixture, injections_fixture};
    let lang_name = |lang: Language| loader.languages.get_index(lang.idx()).unwrap().0.clone();
    let mut layers: Vec<String> = syntax
        .layers
        .iter()
        .map(|(_, layer)| {
            let ranges: Vec<_> = layer
                .ranges
                .iter()
                .map(|r| r.start_byte..r.end_byte)
                .collect();
            format!("{} {ranges:?}", lang_name(layer.language))
        })
        .collect();
    layers.sort();
    let highlight_name = |highlight: Highlight| loader.test_theme.borrow()[highlight.idx()].clone();
    (
        layers,
        injections_fixture("// ", loader, lang_name, syntax, text.into(), ..),
        highlighter_fixture("// ", loader, highlight_name, syntax, text.into(), ..),
    )
}

/// Applies `edits` to `syntax` and checks the result against a fresh parse, like
/// [`assert_matches_fresh`] but also comparing the layers' trees.
#[track_caller]
fn update_matches_fresh(
    loader: &TestLanguageLoader,
    lang: Language,
    syntax: &mut Syntax,
    edits: &[InputEdit],
    text: &str,
    diverged: &mut usize,
) {
    let before = syntax.clone();
    match syntax.update(text.into(), PARSE_TIMEOUT, edits, loader) {
        Ok(()) => {}
        // Slow machines (valgrind) may time out, start over from a fresh parse.
        Err(crate::Error::Timeout) => {
            *syntax = Syntax::new(text.into(), lang, PARSE_TIMEOUT, loader).unwrap()
        }
        Err(err) => panic!("update to {text:?} failed: {err}"),
    }
    let fresh = Syntax::new(text.into(), lang, PARSE_TIMEOUT, loader).unwrap();
    let trees_match = syntax.layers.iter().all(|(_, layer)| {
        fresh_layer_tree(&fresh, layer)
            .is_none_or(|fresh_tree| layer.tree().map(tree_sexp) == Some(tree_sexp(fresh_tree)))
    });
    if trees_match && render(loader, syntax, text) == render(loader, &fresh, text) {
        return;
    }
    if !tree_sitter_diverged(loader, &before, edits, syntax, &fresh, text) {
        assert_matches_fresh(loader, lang, syntax, text);
        panic!("layer trees diverged from a fresh parse of {text:?}");
    }
    *diverged += 1;
    *syntax = fresh;
}

/// The tree of `fresh`'s layer with the same language and ranges as `layer`.
fn fresh_layer_tree<'a>(fresh: &'a Syntax, layer: &LayerData) -> Option<&'a tree_sitter::Tree> {
    fresh
        .layers
        .iter()
        .find(|(_, other)| other.language == layer.language && other.ranges == layer.ranges)
        .and_then(|(_, other)| other.tree())
}

/// The kinds and byte ranges of all of `tree`'s nodes.
fn tree_sexp(tree: &tree_sitter::Tree) -> String {
    fn sexp(node: &tree_sitter::Node, out: &mut String) {
        out.push_str(&format!("({} {:?}", node.kind(), node.byte_range()));
        for child in node.children() {
            sexp(&child, out);
        }
        out.push(')');
    }
    let mut out = String::new();
    sexp(&tree.root_node(), &mut out);
    out
}

/// Whether some layer's tree differs from the fresh parse's tree for the same ranges.
fn tree_sitter_diverged(
    loader: &TestLanguageLoader,
    before: &Syntax,
    edits: &[InputEdit],
    syntax: &Syntax,
    fresh: &Syntax,
    text: &str,
) -> bool {
    let mut diverged = HashSet::new();
    for (idx, layer) in syntax.layers.iter() {
        let (Some(tree), Some(fresh_tree)) = (layer.tree(), fresh_layer_tree(fresh, layer)) else {
            continue;
        };
        let incremental = tree_sexp(tree);
        if incremental == tree_sexp(fresh_tree) {
            continue;
        }
        let Some(old) = before.layers.get(idx) else {
            return false;
        };
        let Some(mut old_tree) = old.tree().cloned() else {
            return false;
        };
        for edit in edits.iter().rev() {
            old_tree.edit(edit);
        }
        // Only an old tree parsed under the same ranges can seed the re-parse.
        let old_ranges = old_tree.included_ranges();
        if old_ranges.len() != layer.ranges.len()
            || old_ranges
                .iter()
                .zip(&layer.ranges)
                .any(|(old, new)| old.start_byte != new.start_byte || old.end_byte != new.end_byte)
        {
            return false;
        }
        // A layer no edit touched keeps its old tree. Error recovery can parse the same text
        // differently at another offset, so that tree may differ from a fresh parse.
        let untouched = old.ranges.iter().all(|range| {
            edits.iter().all(|edit| {
                edit.old_end_byte < range.start_byte || edit.start_byte > range.end_byte
            })
        });
        if !untouched || tree_sexp(&old_tree) != incremental {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_grammar(loader.get_config(layer.language).unwrap().grammar)
                .unwrap();
            parser.set_included_ranges(&layer.ranges).unwrap();
            let reparsed = parser.parse(ropey::RopeSlice::from(text), Some(&old_tree));
            if reparsed.is_none_or(|reparsed| tree_sexp(&reparsed) != incremental) {
                return false;
            }
        }
        diverged.insert(idx);
    }
    let in_diverged_subtree = |mut layer: Layer| loop {
        if diverged.contains(&layer.idx()) {
            return true;
        }
        match syntax.layer(layer).parent {
            Some(parent) => layer = parent,
            None => return false,
        }
    };
    let injected = |syntax: &Syntax, layer: &LayerData| -> Vec<_> {
        let language = |layer| syntax.layer(layer).language;
        let injections = layer.injections.iter();
        injections
            .map(|injection| (injection.range.clone(), language(injection.layer)))
            .collect()
    };
    !diverged.is_empty()
        && syntax.layers.iter().all(|(idx, layer)| {
            in_diverged_subtree(Layer(idx as u32))
                || fresh.layers.iter().any(|(_, other)| {
                    other.language == layer.language
                        && other.ranges == layer.ranges
                        && injected(fresh, other) == injected(syntax, layer)
                })
        })
}

/// A markdown document with multiple nested layers
const LAYERED_DOC: &str = "\
# Title

```rust
fn foo(alpha: u32) -> u32 {
    alpha + 1
}
```

middle text

```rust
fn bar(beta: u32) -> u32 {
    beta * 2
}
```

<!-- a -->

<!-- b -->
";

#[test]
fn unmodified_layers_edit_between_fences() {
    let loader = TestLanguageLoader::new();
    let lang = loader.get("markdown");
    let mut syntax = Syntax::new(LAYERED_DOC.into(), lang, PARSE_TIMEOUT, &loader).unwrap();
    // Insert into "middle text". The first fence is untouched. The second fence and the html
    // layer only move.
    let pos = LAYERED_DOC.find("middle").unwrap() as u32;
    let after = LAYERED_DOC.replace("middle", "muddled");
    syntax
        .update(
            after.as_str().into(),
            PARSE_TIMEOUT,
            &[edit(pos, pos + 6, pos + 7)],
            &loader,
        )
        .unwrap();
    assert_matches_fresh(&loader, lang, &syntax, &after);
}

#[test]
fn unmodified_layers_net_zero_edit() {
    let loader = TestLanguageLoader::new();
    let lang = loader.get("markdown");
    let mut syntax = Syntax::new(LAYERED_DOC.into(), lang, PARSE_TIMEOUT, &loader).unwrap();
    // One transaction: grow the title by one byte and shrink the paragraph by one. The
    // first fence moves right by one, everything after the paragraph nets out to zero.
    let title = LAYERED_DOC.find("Title").unwrap() as u32;
    let text = LAYERED_DOC.find("text").unwrap() as u32;
    let after = LAYERED_DOC
        .replace("Title", "Titles")
        .replace("text", "ext");
    syntax
        .update(
            after.as_str().into(),
            PARSE_TIMEOUT,
            &[
                edit(title + 5, title + 5, title + 6),
                edit(text, text + 1, text),
            ],
            &loader,
        )
        .unwrap();
    assert_matches_fresh(&loader, lang, &syntax, &after);
}

#[test]
fn unmodified_layers_sibling_edit() {
    let loader = TestLanguageLoader::new();
    let lang = loader.get("markdown");
    let mut syntax = Syntax::new(LAYERED_DOC.into(), lang, PARSE_TIMEOUT, &loader).unwrap();
    // Edit inside the first fence (that layer re-parses and rebuilds its locals).
    // the second fence and the html layer are skipped.
    let pos = LAYERED_DOC.find("alpha + 1").unwrap() as u32;
    let after = LAYERED_DOC.replace("alpha + 1", "alpha + 12");
    syntax
        .update(
            after.as_str().into(),
            PARSE_TIMEOUT,
            &[edit(pos + 9, pos + 9, pos + 10)],
            &loader,
        )
        .unwrap();
    assert_matches_fresh(&loader, lang, &syntax, &after);
}

#[test]
fn unmodified_layers_new_fence() {
    let loader = TestLanguageLoader::new();
    let lang = loader.get("markdown");
    let mut syntax = Syntax::new(LAYERED_DOC.into(), lang, PARSE_TIMEOUT, &loader).unwrap();
    // Insert a new fence into the paragraph: the root restructures around it and a
    // new rust layer appears while the surrounding fences are reused.
    let pos = LAYERED_DOC.find("middle text\n").unwrap() as u32 + 12;
    let insertion = "\n```rust\nfn baz() {}\n```\n";
    let mut after = LAYERED_DOC.to_owned();
    after.insert_str(pos as usize, insertion);
    syntax
        .update(
            after.as_str().into(),
            PARSE_TIMEOUT,
            &[edit(pos, pos, pos + insertion.len() as u32)],
            &loader,
        )
        .unwrap();
    assert_matches_fresh(&loader, lang, &syntax, &after);
}

/// Deterministic PRNG so the sweep below is reproducible.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % bound.max(1)
    }
}

/// Randomized equivalence sweep
#[test]
fn edit_sweep() {
    let loader = TestLanguageLoader::new();
    let lang = loader.get("markdown");
    let mut rng = Lcg(0xC0FFEE);
    let mut diverged = 0;
    const BLOCKS: &[&str] = &[
        "plain paragraph text\n",
        "# heading\n",
        "```rust\nfn foo(alpha: u32) -> u32 {\n    alpha + 1\n}\n```\n",
        "```rust\nlet beta = 1;\n```\n",
        "<!-- one -->\n",
        "<!-- two -->\n",
        "```\nplain fence\n```\n",
    ];
    for _ in 0..150 {
        let mut text = String::new();
        for _ in 0..3 + rng.next(4) {
            text.push_str(BLOCKS[rng.next(BLOCKS.len())]);
            text.push('\n');
        }
        let mut syntax = Syntax::new(text.as_str().into(), lang, PARSE_TIMEOUT, &loader).unwrap();
        for _ in 0..5 {
            // Generate 1-3 non-overlapping ascending edits in old-text coordinates.
            let mut edits = Vec::new();
            let mut after = text.clone();
            let mut pos = 0usize;
            let mut delta = 0i64;
            for _ in 0..1 + rng.next(3) {
                if pos + 8 >= text.len() {
                    break;
                }
                let start = pos + rng.next(text.len() - pos - 4);
                let new_pos = |p: usize, delta: i64| (p as i64 + delta) as usize;
                match rng.next(3) {
                    // insert 1-3 chars
                    0 => {
                        let ins = &"xyzq"[..1 + rng.next(3)];
                        after.insert_str(new_pos(start, delta), ins);
                        edits.push(edit(start as u32, start as u32, (start + ins.len()) as u32));
                        delta += ins.len() as i64;
                        pos = start + 4;
                    }
                    // delete 1-2 bytes
                    1 => {
                        let len = 1 + rng.next(2).min(text.len() - start - 1);
                        after.replace_range(new_pos(start, delta)..new_pos(start + len, delta), "");
                        edits.push(edit(start as u32, (start + len) as u32, start as u32));
                        delta -= len as i64;
                        pos = start + len + 4;
                    }
                    // replace 1-2 bytes with 1-3 chars
                    _ => {
                        let len = 1 + rng.next(2).min(text.len() - start - 1);
                        let ins = &"qwer"[..1 + rng.next(3)];
                        after
                            .replace_range(new_pos(start, delta)..new_pos(start + len, delta), ins);
                        edits.push(edit(
                            start as u32,
                            (start + len) as u32,
                            (start + ins.len()) as u32,
                        ));
                        delta += ins.len() as i64 - len as i64;
                        pos = start + len + 4;
                    }
                }
            }
            if edits.is_empty() {
                continue;
            }
            update_matches_fresh(&loader, lang, &mut syntax, &edits, &after, &mut diverged);
            text = after;
        }
    }
    eprintln!("steps where tree-sitter diverged from a fresh parse: {diverged}");
}

#[test]
fn injection_at_range_seam() {
    let loader = TestLanguageLoader::new();
    let lang = loader.get("markdown");
    let text = "<!-- one -->\n\n```\nplain fence\n```\n\n<!-- o -->\n<!--x two -->\n\n";
    let syntax = Syntax::new(text.into(), lang, PARSE_TIMEOUT, &loader).unwrap();
    let fixture = crate::fixtures::injections_fixture(
        "// ",
        &loader,
        |lang| loader.languages.get_index(lang.idx()).unwrap().0.clone(),
        &syntax,
        text.into(),
        ..,
    );
    assert!(
        !fixture.contains("INVALID RANGE"),
        "query iter emitted non-monotonic injection events:\n{fixture}"
    );
}

#[test]
fn injection_crossing_range_seams() {
    let loader = TestLanguageLoader::new();
    let lang = loader.get("markdown");
    // Adjacent html blocks are adjacent ranges of the combined html layer.
    // The html comment opened at the end of the first block only closes in the last one.
    let cases = [
        (
            "<!-- one --><!--\n<!-- two -->\n\n",
            "   <!-- one --><!--
// ┗━━━━━━━━━━━━━━┹─ html comment
   <!-- two -->
// ┡━━━━━━━━━━┛╰─ html
// ╰─ html comment
",
            "   <!-- one --><!--
// ┗━━━━━━━━━━━━━━┹─ comment
   <!-- two -->
// ┗━━━━━━━━━━┹─ comment
",
        ),
        (
            "<!-- one --><!--\n<script></script>\n<!-- two -->\n\n",
            "   <!-- one --><!--
// ┗━━━━━━━━━━━━━━┹─ html comment
   <script></script>
// ┗━━━━━━━━━━━━━━━┹─ html comment
   <!-- two -->
// ┡━━━━━━━━━━┛╰─ html
// ╰─ html comment
",
            "   <!-- one --><!--
// ┗━━━━━━━━━━━━━━┹─ comment
   <script></script>
// ┗━━━━━━━━━━━━━━━┹─ comment
   <!-- two -->
// ┗━━━━━━━━━━┹─ comment
",
        ),
    ];
    for (text, injections, highlights) in cases {
        let syntax = Syntax::new(text.into(), lang, PARSE_TIMEOUT, &loader).unwrap();
        let (_, rendered_injections, rendered_highlights) = render(&loader, &syntax, text);
        assert_eq!(rendered_injections.trim_end(), injections.trim_end());
        assert_eq!(rendered_highlights.trim_end(), highlights.trim_end());
    }
}

#[test]
fn narrowed_query_fence_language() {
    let loader = TestLanguageLoader::new();
    let lang = loader.get("markdown");
    // The fence's language marker lies outside the injected content node.
    let t0 = "text\n\n```rust\nlet x = 1;\n```\n";
    let t1 = "text\n\n```ru\nlet x = 1;\n```\n";
    let pos = t0.find("rust").unwrap() as u32;
    let mut syntax = Syntax::new(t0.into(), lang, PARSE_TIMEOUT, &loader).unwrap();
    syntax
        .update(
            t1.into(),
            PARSE_TIMEOUT,
            &[edit(pos + 2, pos + 4, pos + 2)],
            &loader,
        )
        .unwrap();
    assert_matches_fresh(&loader, lang, &syntax, t1);

    // And the inverse: completing the language name must create the injection.
    let mut syntax = Syntax::new(t1.into(), lang, PARSE_TIMEOUT, &loader).unwrap();
    syntax
        .update(
            t0.into(),
            PARSE_TIMEOUT,
            &[edit(pos + 2, pos + 2, pos + 4)],
            &loader,
        )
        .unwrap();
    assert_matches_fresh(&loader, lang, &syntax, t0);
}

#[test]
fn narrowed_query_bare_fence_language() {
    let loader = TestLanguageLoader::new();
    let lang = loader.get("markdown");
    // The new info string completes a match with the unchanged content node.
    let t0 = "text\n\n```\nlet x = 1;\n```\n";
    let t1 = "text\n\n```rust\nlet x = 1;\n```\n";
    let pos = t0.find("```").unwrap() as u32 + 3;
    let mut syntax = Syntax::new(t0.into(), lang, PARSE_TIMEOUT, &loader).unwrap();
    syntax
        .update(
            t1.into(),
            PARSE_TIMEOUT,
            &[edit(pos, pos, pos + 4)],
            &loader,
        )
        .unwrap();
    assert_matches_fresh(&loader, lang, &syntax, t1);
}

/// Injects html into `html!(..)`. The macro name is captured separately from the token tree.
const HTML_MACRO_INJECTION: &str = r#"
((macro_invocation (identifier) @_macro (token_tree) @injection.content)
 (#eq? @_macro "html")
 (#set! injection.language "html")
 (#set! injection.include-children))"#;

#[test]
fn narrowed_query_predicate() {
    let mut loader = TestLanguageLoader::new();
    loader.overwrite_injections("rust", HTML_MACRO_INJECTION.to_owned());
    let lang = loader.get("rust");
    // Renaming `htmx!` to `html!` changes no structure, so only the macro name is in the
    // re-queried span, not the token tree the match injects into.
    let t0 = "fn pad() { let a = 1; }\nfn main() {\n    htmx!(<p>hello</p>);\n}\n";
    let t1 = "fn pad() { let a = 1; }\nfn main() {\n    html!(<p>hello</p>);\n}\n";
    let pos = t0.find("htmx").unwrap() as u32 + 3;
    let mut syntax = Syntax::new(t0.into(), lang, PARSE_TIMEOUT, &loader).unwrap();
    syntax
        .update(
            t1.into(),
            PARSE_TIMEOUT,
            &[edit(pos, pos + 1, pos + 1)],
            &loader,
        )
        .unwrap();
    assert_matches_fresh(&loader, lang, &syntax, t1);
}

#[test]
fn narrowed_query_shortened_node() {
    let mut loader = TestLanguageLoader::new();
    loader.overwrite_injections(
        "rust",
        r#"((identifier) @injection.content
            (#eq? @injection.content "html")
            (#set! injection.language "comment"))"#
            .to_owned(),
    );
    let lang = loader.get("rust");
    let t0 = "fn pad() { let a = 1; }\nfn main() { let htmlx = 2; }\n";
    let t1 = "fn pad() { let a = 1; }\nfn main() { let html = 2; }\n";
    let t2 = "fn pad() { let a = 1; }\nfn main() { let htm = 2; }\n";
    let pos = t0.find("htmlx").unwrap() as u32;
    let mut syntax = Syntax::new(t0.into(), lang, PARSE_TIMEOUT, &loader).unwrap();
    syntax
        .update(
            t1.into(),
            PARSE_TIMEOUT,
            &[edit(pos + 4, pos + 5, pos + 4)],
            &loader,
        )
        .unwrap();
    assert_matches_fresh(&loader, lang, &syntax, t1);
    syntax
        .update(
            t2.into(),
            PARSE_TIMEOUT,
            &[edit(pos + 3, pos + 4, pos + 3)],
            &loader,
        )
        .unwrap();
    assert_matches_fresh(&loader, lang, &syntax, t2);
}

#[test]
fn narrowed_query_uncaptured_nodes() {
    let mut loader = TestLanguageLoader::new();
    // The match depends on the function body, but only captures the function's name.
    loader.overwrite_injections(
        "rust",
        r#"((function_item
             name: (identifier) @injection.content
             body: (block (expression_statement (macro_invocation))))
            (#set! injection.language "comment"))"#
            .to_owned(),
    );
    let lang = loader.get("rust");
    let t0 = "fn pad() { let a = 1; }\nfn main() {\n    foo!(x);\n    let b = 2;\n}\n";
    let t1 = "fn pad() { let a = 1; }\nfn main() {\n    let b = 2;\n}\n";
    let pos = t0.find("    foo!(x);").unwrap() as u32;
    let mut syntax = Syntax::new(t0.into(), lang, PARSE_TIMEOUT, &loader).unwrap();
    syntax
        .update(
            t1.into(),
            PARSE_TIMEOUT,
            &[edit(pos, pos + 13, pos)],
            &loader,
        )
        .unwrap();
    assert_matches_fresh(&loader, lang, &syntax, t1);
    syntax
        .update(
            t0.into(),
            PARSE_TIMEOUT,
            &[edit(pos, pos, pos + 13)],
            &loader,
        )
        .unwrap();
    assert_matches_fresh(&loader, lang, &syntax, t0);
}

#[test]
fn narrowed_query_combined_pattern() {
    let loader = TestLanguageLoader::new();
    let lang = loader.get("markdown");
    // The second html block becomes the content of an html fence. That match must not
    // reuse the combined html layer, which still holds the first block outside the
    // re-queried span.
    let t0 = "<!-- a -->\n\n<!-- b -->\n\npara\n";
    let t1 = "<!-- a -->\n\n```html\n<!-- b -->\n```\n\npara\n";
    let mut syntax = Syntax::new(t0.into(), lang, PARSE_TIMEOUT, &loader).unwrap();
    syntax
        .update(
            t1.into(),
            PARSE_TIMEOUT,
            &[edit(12, 12, 20), edit(23, 23, 27)],
            &loader,
        )
        .unwrap();
    assert_matches_fresh(&loader, lang, &syntax, t1);
}

#[test]
fn combined_layer_range_growth() {
    let loader = TestLanguageLoader::new();
    let lang = loader.get("rust");
    // Two doc comments form one combined markdown layer. Adding a third one appends a new
    // range to that layer beyond its old tree's span.
    let t0 = "/// alpha\nfn a() {}\n\nfn pad() { let filler = 1; }\n\n/// beta\nfn b() {}\n";
    let insert = "\n/// gamma\nfn c() {}\n";
    let mut t1 = t0.to_owned();
    t1.push_str(insert);
    let pos = t0.len() as u32;
    let mut syntax = Syntax::new(t0.into(), lang, PARSE_TIMEOUT, &loader).unwrap();
    syntax
        .update(
            t1.as_str().into(),
            PARSE_TIMEOUT,
            &[edit(pos, pos, pos + insert.len() as u32)],
            &loader,
        )
        .unwrap();
    assert_matches_fresh(&loader, lang, &syntax, &t1);
}

/// Randomized sweep over rust documents with doc comments and syntax errors.
#[test]
fn rust_edit_sweep() {
    let loader = TestLanguageLoader::new();
    let lang = loader.get("rust");
    let mut rng = Lcg(0xFEEDFACE);
    let mut diverged = 0;
    const PIECES: &[&str] = &[
        "/// alpha beta\n",
        "/// `code` and *emph*\n",
        "///\n",
        "/// # heading\n",
        "fn f() { let x = 1; }\n",
        "\n",
        "// plain comment\n",
        "struct S;\n",
    ];
    const INSERTS: &[&str] = &[
        "/// gamma\n",
        "///x\n",
        "/// ```\n/// let a = 1;\n/// ```\n",
        "\n",
        "fn g() {}\n",
        "x",
        "`",
        "*",
    ];
    for _ in 0..60 {
        let mut text = String::new();
        for _ in 0..2 + rng.next(7) {
            text.push_str(PIECES[rng.next(PIECES.len())]);
        }
        let mut syntax = Syntax::new(text.as_str().into(), lang, PARSE_TIMEOUT, &loader).unwrap();
        for _round in 0..6 {
            let mut after = text.clone();
            let e;
            if rng.next(2) == 0 {
                // insert a whole piece at a line boundary
                let lines: Vec<usize> = std::iter::once(0)
                    .chain(text.match_indices('\n').map(|(i, _)| i + 1))
                    .collect();
                let at = lines[rng.next(lines.len())];
                let ins = INSERTS[rng.next(INSERTS.len())];
                after.insert_str(at, ins);
                e = edit(at as u32, at as u32, (at + ins.len()) as u32);
            } else if text.len() > 8 {
                // delete a small random range
                let start = rng.next(text.len() - 4);
                let len = 1 + rng.next(4).min(text.len() - start - 1);
                after.replace_range(start..start + len, "");
                e = edit(start as u32, (start + len) as u32, start as u32);
            } else {
                continue;
            };
            update_matches_fresh(&loader, lang, &mut syntax, &[e], &after, &mut diverged);
            text = after;
        }
    }
    eprintln!("steps where tree-sitter diverged from a fresh parse: {diverged}");
}

#[test]
fn reparse_seeded_by_old_tree() {
    let loader = TestLanguageLoader::new();
    let markdown = loader.get("markdown");
    let rust = loader.get("rust");
    // An edit inside the fence re-parses the markdown and rust layers from their old trees.
    let t0 = "# Title\n\n```rust\nfn f(x: u32) -> u32 {\n    x + 1\n}\n```\n\ntext\n";
    let pos = t0.find("x + 1").unwrap();
    let mut t1 = t0.to_owned();
    t1.insert(pos, 'y');
    let pos = pos as u32;
    let mut syntax = Syntax::new(t0.into(), markdown, PARSE_TIMEOUT, &loader).unwrap();
    let before = [markdown, rust].map(|lang| node_ids(&syntax, lang));
    syntax
        .update(
            t1.as_str().into(),
            PARSE_TIMEOUT,
            &[edit(pos, pos, pos + 1)],
            &loader,
        )
        .unwrap();
    // A seeded re-parse reuses some of the old tree's nodes.
    for (lang, old) in [markdown, rust].into_iter().zip(before) {
        let new = node_ids(&syntax, lang);
        assert_ne!(new, old, "the layer was not re-parsed");
        assert!(
            !new.is_disjoint(&old),
            "the re-parse did not reuse the old tree"
        );
    }
}

/// The ids of all nodes in the `lang` layer's tree.
fn node_ids(syntax: &Syntax, lang: Language) -> HashSet<usize> {
    fn walk(node: tree_sitter::Node, ids: &mut HashSet<usize>) {
        ids.insert(node.id());
        for child in node.children() {
            walk(child, ids);
        }
    }
    let (_, layer) = syntax
        .layers
        .iter()
        .find(|(_, layer)| layer.language == lang)
        .unwrap();
    let mut ids = HashSet::new();
    walk(layer.parse_tree.as_ref().unwrap().root_node(), &mut ids);
    ids
}

/// Update-path benchmark, run with
/// `cargo test --release -p tree-house bench_update -- --ignored --nocapture`.
/// `BENCH_N` and `BENCH_FRAMES` override the iteration counts.
#[test]
#[ignore]
fn bench_update() {
    let loader = TestLanguageLoader::new();
    let lang = loader.get("markdown");
    let env = |name: &str, default: u32| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let n = env("BENCH_N", 200);
    // Alternate between inserts and deletes at `pos`
    let toggle = |syntax: &mut Syntax, text: &mut String, pos: u32| {
        let start = std::time::Instant::now();
        for i in 0..n {
            let e = if i % 2 == 0 {
                text.insert(pos as usize, 'y');
                edit(pos, pos, pos + 1)
            } else {
                text.remove(pos as usize);
                edit(pos, pos + 1, pos)
            };
            syntax
                .update(text.as_str().into(), PARSE_TIMEOUT, &[e], &loader)
                .unwrap();
        }
        start.elapsed() / n
    };

    let mut text = String::new();
    for i in 0..300 {
        text.push_str(&format!(
            "## Section {i}\n\nsome paragraph text with `inline code` and *emph*\n\n\
             ```rust\nfn f{i}(x: u32) -> u32 {{\n    x + {i}\n}}\n```\n\n"
        ));
    }
    eprintln!("doc: {} bytes, 300 rust fences", text.len());
    let mut syntax = Syntax::new(text.as_str().into(), lang, PARSE_TIMEOUT, &loader).unwrap();
    let pos = text.find("x + 150").unwrap() as u32;
    eprintln!(
        "fence edit: {:?}/update",
        toggle(&mut syntax, &mut text, pos)
    );
    let pos = text.find("some paragraph text with `inline code` and *emph*\n\n```rust\nfn f150");
    let pos = pos.unwrap() as u32;
    eprintln!(
        "paragraph edit: {:?}/update",
        toggle(&mut syntax, &mut text, pos)
    );

    // construct a highlighter and advance it over a 4KB viewport
    let rope = ropey::Rope::from_str(&text);
    let viewport_start = text.find("## Section 150").unwrap() as u32;
    let viewport = viewport_start..(viewport_start + 4000).min(text.len() as u32);
    let frames = env("BENCH_FRAMES", 500);
    let start = std::time::Instant::now();
    let mut events = 0;
    for _ in 0..frames {
        let mut highlighter = Highlighter::new(&syntax, rope.slice(..), &loader, viewport.clone());
        while highlighter.next_event_offset() != u32::MAX {
            events += highlighter.advance().1.count();
        }
    }
    eprintln!(
        "render 4KB viewport: {:?}/frame ({events} events)",
        start.elapsed() / frames
    );
}

/// Randomized sweep of multi-edit transactions (1-3 edits each) with edits around injection boundaries.
#[test]
fn multi_edit_sweep() {
    const MARKDOWN_PIECES: &[&str] = &[
        "plain paragraph text\n\n",
        "# heading\n\n",
        "```rust\nfn foo(alpha: u32) -> u32 {\n    alpha + 1\n}\n```\n\n",
        "```rust\nlet beta = 1;\n```\n\n",
        "<!-- one -->\n",
        "<!-- two -->\n\n",
        "```\nplain fence\n```\n\n",
        "text with `code` and *emph*\n\n",
        "```html\n<b>x</b><!-- c -->\n```\n\n",
        "```rust\n/// ```rust\n/// let x = 1;\n/// ```\nfn g() {}\n```\n\n",
    ];
    const MARKDOWN_INSERTS: &[&str] = &[
        "x",
        "`",
        "*",
        "\n",
        "```",
        "```rust\n",
        "<!-- z -->\n",
        "rust",
        "-->",
        "<!--",
        "\n\n",
    ];
    const RUST_PIECES: &[&str] = &[
        "/// alpha beta\n",
        "/// `code` and *emph*\n",
        "///\n",
        "/// ```\n/// let a = 1;\n/// ```\n",
        "fn f(a: u32) { let x = a; }\n",
        "\n",
        "// plain comment\n",
        "struct S;\n",
        "impl S {\n    /// inner doc\n    fn m(&self) {}\n}\n",
        "html!(<p>hi</p>);\n",
    ];
    const RUST_INSERTS: &[&str] = &[
        "/// gamma\n",
        "///x\n",
        "\n",
        "fn g() {}\n",
        "x",
        "`",
        "*",
        "/",
        "{",
        "}",
        " ",
        "//",
        "ml",
    ];
    const MARKERS: &[&str] = &[
        "\n", "///", "```", "<!--", "-->", "`", "*", "{", "}", "html",
    ];

    let loader = TestLanguageLoader::new();
    let mut rng = Lcg(0x5EED);
    let mut diverged = 0;
    for (lang, pieces, inserts) in [
        ("markdown", MARKDOWN_PIECES, MARKDOWN_INSERTS),
        ("rust", RUST_PIECES, RUST_INSERTS),
    ] {
        let lang = loader.get(lang);
        for _ in 0..30 {
            let mut text = String::new();
            for _ in 0..2 + rng.next(8) {
                text.push_str(pieces[rng.next(pieces.len())]);
            }
            let mut syntax =
                Syntax::new(text.as_str().into(), lang, PARSE_TIMEOUT, &loader).unwrap();
            for _ in 0..8 {
                // Ascending, non-overlapping edits in old-text coordinates.
                let mut edits = Vec::new();
                let mut after = String::new();
                let mut copied = 0;
                let mut lo = 0;
                for _ in 0..1 + rng.next(3) {
                    if lo >= text.len() {
                        break;
                    }
                    let marks: Vec<usize> = MARKERS
                        .iter()
                        .flat_map(|m| text.match_indices(m).flat_map(|(i, m)| [i, i + m.len()]))
                        .filter(|&i| i >= lo && i <= text.len())
                        .collect();
                    let start = if !marks.is_empty() && rng.next(3) != 0 {
                        marks[rng.next(marks.len())]
                    } else {
                        lo + rng.next(text.len() - lo + 1)
                    };
                    let kind = rng.next(3); // insert, delete or replace
                    let end = if kind == 0 {
                        start
                    } else {
                        let len = if rng.next(4) == 0 { 12 } else { 3 };
                        (start + 1 + rng.next(len)).min(text.len())
                    };
                    let ins = if kind == 1 {
                        ""
                    } else {
                        inserts[rng.next(inserts.len())]
                    };
                    if start == end && ins.is_empty() {
                        continue;
                    }
                    after.push_str(&text[copied..start]);
                    after.push_str(ins);
                    edits.push(edit(start as u32, end as u32, (start + ins.len()) as u32));
                    copied = end;
                    lo = end + 1;
                }
                if edits.is_empty() {
                    continue;
                }
                after.push_str(&text[copied..]);
                update_matches_fresh(&loader, lang, &mut syntax, &edits, &after, &mut diverged);
                text = after;
            }
        }
    }
    eprintln!("steps where tree-sitter diverged from a fresh parse: {diverged}");
}
