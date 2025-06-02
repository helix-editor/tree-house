use std::cmp::Reverse;
use std::iter::{self, Peekable};
use std::mem::take;
use std::sync::Arc;

use arc_swap::ArcSwap;
use hashbrown::{HashMap, HashSet};
use once_cell::sync::Lazy;
use regex_cursor::engines::meta::Regex;
use ropey::{LineType::LF_CR, RopeSlice};

use crate::config::{LanguageConfig, LanguageLoader};
use crate::highlighter::Highlight;
use crate::locals::Locals;
use crate::parse::LayerUpdateFlags;
use crate::{Injection, Language, Layer, LayerData, Range, Syntax, TREE_SITTER_MATCH_LIMIT};
use tree_sitter::{
    query::{self, InvalidPredicateError, UserPredicate},
    Capture, Grammar, InactiveQueryCursor, MatchedNodeIdx, Node, Pattern, Query, QueryMatch,
};

const SHEBANG: &str = r"#!\s*(?:\S*[/\\](?:env\s+(?:\-\S+\s+)*)?)?([^\s\.\d]+)";
static SHEBANG_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(SHEBANG).unwrap());

#[derive(Clone, Default, Debug)]
pub struct InjectionProperties {
    include_children: IncludedChildren,
    language: Option<Box<str>>,
    combined: bool,
}

/// An indicator in the document or query source file which used by the loader to know which
/// language an injection should use.
///
/// For example if a query sets a property `(#set! injection.language "rust")` then the loader
/// should load the Rust language. Alternatively the loader might be asked to load a language
/// based on some text in the document, for example a markdown code fence language name.
#[derive(Debug, Clone, Copy)]
pub enum InjectionLanguageMarker<'a> {
    /// The language is specified by name in the injection query itself.
    ///
    /// For example `(#set! injection.language "rust")`. These names should match exactly and so
    /// they can be looked up by equality - very efficiently.
    Name(&'a str),
    /// The language is specified by name - or similar - within the parsed document.
    ///
    /// This is slightly different than the `ExactName` variant: within a document you might
    /// specify Markdown as "md" or "markdown" for example. The loader should look up the language
    /// name by longest matching regex.
    Match(RopeSlice<'a>),
    Filename(RopeSlice<'a>),
    Shebang(RopeSlice<'a>),
}

#[derive(Clone, Debug)]
pub struct InjectionQueryMatch<'tree> {
    include_children: IncludedChildren,
    language: Language,
    scope: Option<InjectionScope>,
    node: Node<'tree>,
    last_match: bool,
    pattern: Pattern,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum InjectionScope {
    Match {
        id: u32,
    },
    Pattern {
        pattern: Pattern,
        language: Language,
    },
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
enum IncludedChildren {
    #[default]
    None,
    All,
    Unnamed,
}

#[derive(Debug)]
pub struct InjectionsQuery {
    injection_query: Query,
    injection_properties: HashMap<Pattern, InjectionProperties>,
    injection_content_capture: Option<Capture>,
    injection_language_capture: Option<Capture>,
    injection_filename_capture: Option<Capture>,
    injection_shebang_capture: Option<Capture>,
    // Note that the injections query is concatenated with the locals query.
    pub(crate) local_query: Query,
    // TODO: Use a Vec<bool> instead?
    pub(crate) not_scope_inherits: HashSet<Pattern>,
    pub(crate) local_scope_capture: Option<Capture>,
    pub(crate) local_definition_captures: ArcSwap<HashMap<Capture, Highlight>>,
}

impl InjectionsQuery {
    pub fn new(
        grammar: Grammar,
        injection_query_text: &str,
        local_query_text: &str,
    ) -> Result<Self, query::ParseError> {
        let mut query_source =
            String::with_capacity(injection_query_text.len() + local_query_text.len());
        query_source.push_str(injection_query_text);
        query_source.push_str(local_query_text);

        let mut injection_properties: HashMap<Pattern, InjectionProperties> = HashMap::new();
        let mut not_scope_inherits = HashSet::new();
        let injection_query = Query::new(grammar, injection_query_text, |pattern, predicate| {
            match predicate {
                // injections
                UserPredicate::SetProperty {
                    key: "injection.include-unnamed-children",
                    val: None,
                } => {
                    injection_properties
                        .entry(pattern)
                        .or_default()
                        .include_children = IncludedChildren::Unnamed
                }
                UserPredicate::SetProperty {
                    key: "injection.include-children",
                    val: None,
                } => {
                    injection_properties
                        .entry(pattern)
                        .or_default()
                        .include_children = IncludedChildren::All
                }
                UserPredicate::SetProperty {
                    key: "injection.language",
                    val: Some(lang),
                } => injection_properties.entry(pattern).or_default().language = Some(lang.into()),
                UserPredicate::SetProperty {
                    key: "injection.combined",
                    val: None,
                } => injection_properties.entry(pattern).or_default().combined = true,
                predicate => {
                    return Err(InvalidPredicateError::unknown(predicate));
                }
            }
            Ok(())
        })?;
        let mut local_query = Query::new(grammar, local_query_text, |pattern, predicate| {
            match predicate {
                UserPredicate::SetProperty {
                    key: "local.scope-inherits",
                    val,
                } => {
                    if val.is_some_and(|val| val != "true") {
                        not_scope_inherits.insert(pattern);
                    }
                }
                predicate => {
                    return Err(InvalidPredicateError::unknown(predicate));
                }
            }
            Ok(())
        })?;

        // The injection queries do not track references - these are read by the highlight
        // query instead.
        local_query.disable_capture("local.reference");

        Ok(InjectionsQuery {
            injection_properties,
            injection_content_capture: injection_query.get_capture("injection.content"),
            injection_language_capture: injection_query.get_capture("injection.language"),
            injection_filename_capture: injection_query.get_capture("injection.filename"),
            injection_shebang_capture: injection_query.get_capture("injection.shebang"),
            injection_query,
            not_scope_inherits,
            local_scope_capture: local_query.get_capture("local.scope"),
            local_definition_captures: ArcSwap::from_pointee(HashMap::new()),
            local_query,
        })
    }

    pub(crate) fn configure(&self, f: &mut impl FnMut(&str) -> Option<Highlight>) {
        let local_definition_captures = self
            .local_query
            .captures()
            .filter_map(|(capture, name)| {
                let suffix = name.strip_prefix("local.definition.")?;
                Some((capture, f(suffix)?))
            })
            .collect();
        self.local_definition_captures
            .store(Arc::new(local_definition_captures));
    }

    fn process_match<'a, 'tree>(
        &self,
        query_match: &QueryMatch<'a, 'tree>,
        node_idx: MatchedNodeIdx,
        source: RopeSlice<'a>,
        loader: impl LanguageLoader,
    ) -> Option<InjectionQueryMatch<'tree>> {
        let properties = self.injection_properties.get(&query_match.pattern());

        let mut marker = None;
        let mut last_content_node = 0;
        let mut content_nodes = 0;
        for (i, matched_node) in query_match.matched_nodes().enumerate() {
            let capture = Some(matched_node.capture);
            if capture == self.injection_language_capture {
                let range = matched_node.node.byte_range();
                marker = Some(InjectionLanguageMarker::Match(
                    source.slice(range.start as usize..range.end as usize),
                ));
            } else if capture == self.injection_filename_capture {
                let range = matched_node.node.byte_range();
                marker = Some(InjectionLanguageMarker::Filename(
                    source.slice(range.start as usize..range.end as usize),
                ));
            } else if capture == self.injection_shebang_capture {
                let range = matched_node.node.byte_range();
                let node_slice = source.slice(range.start as usize..range.end as usize);

                // some languages allow space and newlines before the actual string content
                // so a shebang could be on either the first or second line
                let lines = if node_slice.len_lines(LF_CR) >= 2 {
                    let end = node_slice.line_to_byte_idx(2, LF_CR);
                    node_slice.slice(..end)
                } else {
                    node_slice
                };

                marker = SHEBANG_REGEX
                    .captures_iter(regex_cursor::Input::new(lines))
                    .map(|cap| {
                        let cap = lines.slice(cap.get_group(1).unwrap().range());
                        InjectionLanguageMarker::Shebang(cap)
                    })
                    .next()
            } else if capture == self.injection_content_capture {
                content_nodes += 1;

                last_content_node = i as u32;
            }
        }
        let marker = marker.or(properties
            .and_then(|p| p.language.as_deref())
            .map(InjectionLanguageMarker::Name))?;

        let language = loader.language_for_marker(marker)?;
        let scope = if properties.is_some_and(|p| p.combined) {
            Some(InjectionScope::Pattern {
                pattern: query_match.pattern(),
                language,
            })
        } else if content_nodes != 1 {
            Some(InjectionScope::Match {
                id: query_match.id(),
            })
        } else {
            None
        };

        Some(InjectionQueryMatch {
            language,
            scope,
            include_children: properties.map(|p| p.include_children).unwrap_or_default(),
            node: query_match.matched_node(node_idx).node.clone(),
            last_match: last_content_node == node_idx,
            pattern: query_match.pattern(),
        })
    }

    /// Executes the query on the given input and return an iterator of
    /// injection ranges together with their injection properties
    ///
    /// The ranges yielded by the iterator have an ascending start range.
    /// The ranges do not overlap exactly (matches of the exact same node are
    /// resolved with normal precedence rules). However, ranges can be nested.
    /// For example:
    ///
    /// ``` no-compile
    ///   | range 2 |
    /// |   range 1  |
    /// ```
    /// is possible and will always result in iteration order [range1, range2].
    /// This case should be handled by the calling function
    fn execute<'a>(
        &'a self,
        node: &Node<'a>,
        source: RopeSlice<'a>,
        loader: &'a impl LanguageLoader,
    ) -> impl Iterator<Item = InjectionQueryMatch<'a>> + 'a {
        let mut cursor = InactiveQueryCursor::new(0..u32::MAX, TREE_SITTER_MATCH_LIMIT)
            .execute_query(&self.injection_query, node, source);
        let injection_content_capture = self.injection_content_capture.unwrap();
        let iter = iter::from_fn(move || loop {
            let (query_match, node_idx) = cursor.next_matched_node()?;
            if query_match.matched_node(node_idx).capture != injection_content_capture {
                continue;
            }
            let Some(mat) = self.process_match(&query_match, node_idx, source, loader) else {
                query_match.remove();
                continue;
            };
            let range = query_match.matched_node(node_idx).node.byte_range();
            if mat.last_match {
                query_match.remove();
            }
            if range.is_empty() {
                continue;
            }
            break Some(mat);
        });
        let mut buf = Vec::new();
        let mut iter = iter.peekable();
        // handle identical/overlapping matches to correctly account for precedence
        iter::from_fn(move || {
            if let Some(mat) = buf.pop() {
                return Some(mat);
            }
            let mut res = iter.next()?;
            // if children are not included then nested injections don't
            // interfere with each other unless exactly identical. Since
            // this is the default setting we have a fastpath for it
            if res.include_children == IncludedChildren::None {
                let mut fast_return = true;
                while let Some(overlap) =
                    iter.next_if(|mat| mat.node.byte_range() == res.node.byte_range())
                {
                    if overlap.include_children != IncludedChildren::None {
                        buf.push(overlap);
                        fast_return = false;
                        break;
                    }
                    // Prefer the last capture which matches this exact node.
                    res = overlap;
                }
                if fast_return {
                    return Some(res);
                }
            }

            // we if can't use the fastpath we accumulate all overlapping matches
            // and then sort them according to precedence rules...
            while let Some(overlap) = iter.next_if(|mat| mat.node.end_byte() <= res.node.end_byte())
            {
                buf.push(overlap)
            }
            if buf.is_empty() {
                return Some(res);
            }
            buf.push(res);
            buf.sort_unstable_by_key(|mat| (mat.pattern, Reverse(mat.node.start_byte())));
            buf.pop()
        })
    }
}

impl Syntax {
    pub(crate) fn run_injection_query(
        &mut self,
        layer: Layer,
        edits: &[tree_sitter::InputEdit],
        source: RopeSlice<'_>,
        loader: &impl LanguageLoader,
        mut parse_layer: impl FnMut(Layer),
    ) {
        self.map_injections(layer, None, edits);
        let layer_data = &mut self.layer_mut(layer);
        let Some(LanguageConfig {
            injection_query: ref injections_query,
            ..
        }) = loader.get_config(layer_data.language)
        else {
            return;
        };
        if injections_query.injection_content_capture.is_none() {
            return;
        }

        // work around borrow checker
        let parent_ranges = take(&mut layer_data.ranges);
        let parse_tree = layer_data.parse_tree.take().unwrap();
        let mut injections: Vec<Injection> = Vec::with_capacity(layer_data.injections.len());
        let mut old_injections = take(&mut layer_data.injections).into_iter().peekable();

        let injection_query = injections_query.execute(&parse_tree.root_node(), source, loader);

        let mut combined_injections: HashMap<InjectionScope, Layer> = HashMap::with_capacity(32);
        for mat in injection_query {
            let matched_node_range = mat.node.byte_range();
            let mut insert_position = injections.len();
            // if a parent node already has an injection ignore this injection
            // in theory the first condition would be enough to detect that
            // however in case the parent node does not include children it
            // is possible that one of these children is another separate
            // injection. In these cases we cannot skip the injection
            //
            // also the precedence sorting (and rare intersection) means that
            // overlapping injections may be sorted not by position but by
            // precedence (highest precedence first). the code here ensures
            // that injections get sorted to the correct position
            if let Some(last_injection) = injections
                .last()
                .filter(|injection| ranges_intersect(&injection.range, &matched_node_range))
            {
                // this condition is not needed but serves as fast path
                // for common cases
                if last_injection.range.start <= matched_node_range.start {
                    continue;
                } else {
                    insert_position = injections.partition_point(|injection| {
                        injection.range.end <= matched_node_range.start
                    });
                    if injections[insert_position].range.start < matched_node_range.end {
                        continue;
                    }
                }
            }

            let language = mat.language;
            let reused_injection =
                self.reuse_injection(language, matched_node_range.clone(), &mut old_injections);
            let layer = match mat.scope {
                Some(scope @ InjectionScope::Match { .. }) if mat.last_match => {
                    combined_injections.remove(&scope).unwrap_or_else(|| {
                        self.init_injection(layer, mat.language, reused_injection.clone())
                    })
                }
                Some(scope) => *combined_injections.entry(scope).or_insert_with(|| {
                    self.init_injection(layer, mat.language, reused_injection.clone())
                }),
                None => self.init_injection(layer, mat.language, reused_injection.clone()),
            };
            let mut layer_data = self.layer_mut(layer);
            if !layer_data.flags.touched {
                layer_data.flags.touched = true;
                parse_layer(layer)
            }
            if layer_data.flags.reused {
                layer_data.flags.modified |= reused_injection.as_ref().map_or(true, |injection| {
                    injection.matched_node_range != matched_node_range || injection.layer != layer
                });
            } else if let Some(reused_injection) = reused_injection {
                layer_data.flags.reused = true;
                layer_data.flags.modified = true;
                let reused_parse_tree = self.layer(reused_injection.layer).tree().cloned();
                layer_data = self.layer_mut(layer);
                layer_data.parse_tree = reused_parse_tree;
            }

            let old_len = injections.len();
            intersect_ranges(mat.include_children, mat.node, &parent_ranges, |range| {
                layer_data.ranges.push(tree_sitter::Range {
                    start_point: tree_sitter::Point::ZERO,
                    end_point: tree_sitter::Point::ZERO,
                    start_byte: range.start,
                    end_byte: range.end,
                });
                injections.push(Injection {
                    range,
                    layer,
                    matched_node_range: matched_node_range.clone(),
                });
            });
            if old_len != insert_position {
                let inserted = injections.len() - old_len;
                injections[insert_position..].rotate_right(inserted);
                layer_data.ranges[insert_position..].rotate_right(inserted);
            }
        }

        // Any remaining injections which were not reused should have their layers marked as
        // modified. These layers might have a new set of ranges (if they were visited) and so
        // their trees need to be re-parsed.
        for old_injection in old_injections {
            self.layer_mut(old_injection.layer).flags.modified = true;
        }

        let layer_data = &mut self.layer_mut(layer);
        layer_data.ranges = parent_ranges;
        layer_data.parse_tree = Some(parse_tree);
        layer_data.injections = injections;
    }

    /// Maps the layers injection ranges through edits to enable incremental re-parsing.
    fn map_injections(
        &mut self,
        layer: Layer,
        // TODO: drop this parameter?
        offset: Option<i32>,
        mut edits: &[tree_sitter::InputEdit],
    ) {
        if edits.is_empty() && offset.unwrap_or(0) == 0 {
            return;
        }
        let layer_data = self.layer_mut(layer);
        let first_relevant_injection = layer_data
            .injections
            .partition_point(|injection| injection.range.end < edits[0].start_byte);
        if first_relevant_injection == layer_data.injections.len() {
            return;
        }
        let mut offset = if let Some(offset) = offset {
            let first_relevant_edit = edits.partition_point(|edit| {
                (edit.old_end_byte as i32) < (layer_data.ranges[0].end_byte as i32 - offset)
            });
            edits = &edits[first_relevant_edit..];
            offset
        } else {
            0
        };
        // injections and edits are non-overlapping and sorted so we can
        // apply edits in O(M+N) instead of O(NM)
        let mut edits = edits.iter().peekable();
        let mut injections = take(&mut layer_data.injections);
        for injection in &mut injections[first_relevant_injection..] {
            let injection_range = &mut injection.range;
            let matched_node_range = &mut injection.matched_node_range;
            let flags = &mut self.layer_mut(injection.layer).flags;

            debug_assert!(matched_node_range.start <= injection_range.start);
            debug_assert!(matched_node_range.end >= injection_range.end);

            while let Some(edit) =
                edits.next_if(|edit| edit.old_end_byte < matched_node_range.start)
            {
                offset += edit.offset();
            }
            let mut mapped_node_range_start = (matched_node_range.start as i32 + offset) as u32;
            if let Some(edit) = edits
                .peek()
                .filter(|edit| edit.start_byte <= matched_node_range.start)
            {
                mapped_node_range_start = (edit.new_end_byte as i32 + offset) as u32;
            }
            while let Some(edit) = edits.next_if(|edit| edit.old_end_byte < injection_range.start) {
                offset += edit.offset();
            }
            flags.moved = offset != 0;
            let mut mapped_start = (injection_range.start as i32 + offset) as u32;
            if let Some(edit) = edits.next_if(|edit| edit.old_end_byte <= injection_range.end) {
                if edit.start_byte < injection_range.start {
                    flags.moved = true;
                    mapped_start = (edit.new_end_byte as i32 + offset) as u32;
                } else {
                    flags.modified = true;
                }
                offset += edit.offset();
                while let Some(edit) =
                    edits.next_if(|edit| edit.old_end_byte <= injection_range.end)
                {
                    offset += edit.offset();
                }
            }
            let mut mapped_end = (injection_range.end as i32 + offset) as u32;
            if let Some(edit) = edits
                .peek()
                .filter(|edit| edit.start_byte <= injection_range.end)
            {
                flags.modified = true;

                if edit.start_byte < injection_range.start {
                    mapped_start = (edit.new_end_byte as i32 + offset) as u32;
                    mapped_end = mapped_start;
                }
            }
            let mut mapped_node_range_end = (matched_node_range.end as i32 + offset) as u32;
            if let Some(edit) = edits
                .peek()
                .filter(|edit| edit.start_byte <= matched_node_range.end)
            {
                if edit.start_byte < matched_node_range.start {
                    mapped_node_range_start = (edit.new_end_byte as i32 + offset) as u32;
                    mapped_node_range_end = mapped_node_range_start;
                }
            }
            *injection_range = mapped_start..mapped_end;
            *matched_node_range = mapped_node_range_start..mapped_node_range_end;
        }
        self.layer_mut(layer).injections = injections;
    }

    fn init_injection(
        &mut self,
        parent: Layer,
        language: Language,
        reuse: Option<Injection>,
    ) -> Layer {
        match reuse {
            Some(old_injection) => {
                let layer_data = self.layer_mut(old_injection.layer);
                debug_assert_eq!(layer_data.parent, Some(parent));
                layer_data.flags.reused = true;
                layer_data.ranges.clear();
                old_injection.layer
            }
            None => {
                let layer = self.layers.insert(LayerData {
                    language,
                    parse_tree: None,
                    ranges: Vec::new(),
                    injections: Vec::new(),
                    flags: LayerUpdateFlags::default(),
                    parent: Some(parent),
                    locals: Locals::default(),
                });
                Layer(layer as u32)
            }
        }
    }

    // TODO: only reuse if same pattern is matched
    fn reuse_injection(
        &mut self,
        language: Language,
        new_range: Range,
        injections: &mut Peekable<impl Iterator<Item = Injection>>,
    ) -> Option<Injection> {
        while let Some(skipped) =
            injections.next_if(|injection| injection.range.end <= new_range.start)
        {
            // If the layer had an injection and now does not have the injection, consider the
            // skipped layer to be modified so that its tree is re-parsed. It must be re-parsed
            // since the skipped layer now has a different set of ranges than it used to. Note
            // that the layer isn't marked as `touched` so it could be discarded if the layer
            // is not ever visited.
            self.layer_mut(skipped.layer).flags.modified = true;
        }
        injections
            .next_if(|injection| {
                injection.range.start < new_range.end
                    && self.layer(injection.layer).language == language
                    && !self.layer(injection.layer).flags.reused
            })
            .clone()
    }
}

fn intersect_ranges(
    include_children: IncludedChildren,
    node: Node,
    parent_ranges: &[tree_sitter::Range],
    push_range: impl FnMut(Range),
) {
    let range = node.byte_range();
    let i = parent_ranges.partition_point(|parent_range| parent_range.end_byte <= range.start);
    let parent_ranges = parent_ranges[i..]
        .iter()
        .map(|range| range.start_byte..range.end_byte);
    match include_children {
        IncludedChildren::None => intersect_ranges_impl(
            range,
            node.children().map(|node| node.byte_range()),
            parent_ranges,
            push_range,
        ),
        IncludedChildren::All => {
            intersect_ranges_impl(range, [].into_iter(), parent_ranges, push_range)
        }
        IncludedChildren::Unnamed => intersect_ranges_impl(
            range,
            node.children()
                .filter(|node| node.is_named())
                .map(|node| node.byte_range()),
            parent_ranges,
            push_range,
        ),
    }
}

fn intersect_ranges_impl(
    range: Range,
    excluded_ranges: impl Iterator<Item = Range>,
    parent_ranges: impl Iterator<Item = Range>,
    mut push_range: impl FnMut(Range),
) {
    let mut start = range.start;
    let mut excluded_ranges = excluded_ranges.filter(|range| !range.is_empty()).peekable();
    let mut parent_ranges = parent_ranges.peekable();
    loop {
        let parent_range = parent_ranges.peek().unwrap().clone();
        if let Some(excluded_range) =
            excluded_ranges.next_if(|range| range.start <= parent_range.end)
        {
            if excluded_range.start >= range.end {
                break;
            }
            if start != excluded_range.start {
                push_range(start..excluded_range.start)
            }
            start = excluded_range.end;
        } else {
            parent_ranges.next();
            if parent_range.end >= range.end {
                break;
            }
            if start != parent_range.end {
                push_range(start..parent_range.end)
            }
            let Some(next_parent_range) = parent_ranges.peek() else {
                return;
            };
            start = next_parent_range.start;
        }
    }
    if start != range.end {
        push_range(start..range.end)
    }
}

fn ranges_intersect(a: &Range, b: &Range) -> bool {
    // Adapted from <https://github.com/helix-editor/helix/blob/8df58b2e1779dcf0046fb51ae1893c1eebf01e7c/helix-core/src/selection.rs#L156-L163>
    a.start == b.start || (a.end > b.start && b.end > a.start)
}
