use std::cmp::Reverse;
use std::iter::{self, from_fn, Peekable};
use std::mem::take;
use std::sync::Arc;

use arc_swap::ArcSwap;
use hashbrown::{HashMap, HashSet};
use regex_cursor::engines::meta::Regex;
use ropey::RopeSlice;
use std::sync::LazyLock;

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
static SHEBANG_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(SHEBANG).unwrap());

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
    /// The range of the node the match is rooted at, see [`Injection::pattern_range`].
    extent: Range,
}

/// The layer identity shared by the entries of one injection.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum InjectionScope {
    /// Per-run match id for a multi-content node match
    Match { id: u32 },
    /// Pattern for `injection.combined`
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
    /// Per pattern, see [`Query::pattern_capture_depths`].
    capture_depths: Box<[Vec<(Capture, u32)>]>,
    pub(crate) local_query: Query,
    /// Per locals pattern, see [`Query::pattern_capture_depths`].
    pub(crate) local_capture_depths: Box<[Vec<(Capture, u32)>]>,
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
            capture_depths: injection_query
                .patterns()
                .map(|pattern| injection_query.pattern_capture_depths(pattern))
                .collect(),
            injection_query,
            not_scope_inherits,
            local_scope_capture: local_query.get_capture("local.scope"),
            local_definition_captures: ArcSwap::from_pointee(HashMap::new()),
            local_capture_depths: local_query
                .patterns()
                .map(|pattern| local_query.pattern_capture_depths(pattern))
                .collect(),
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
                    source.byte_slice(range.start as usize..range.end as usize),
                ));
            } else if capture == self.injection_filename_capture {
                let range = matched_node.node.byte_range();
                marker = Some(InjectionLanguageMarker::Filename(
                    source.byte_slice(range.start as usize..range.end as usize),
                ));
            } else if capture == self.injection_shebang_capture {
                let range = matched_node.node.byte_range();
                let node_slice = source.byte_slice(range.start as usize..range.end as usize);

                // some languages allow space and newlines before the actual string content
                // so a shebang could be on either the first or second line
                let lines = if let Ok(end) = node_slice.try_line_to_byte(2) {
                    node_slice.byte_slice(..end)
                } else {
                    node_slice
                };

                marker = SHEBANG_REGEX
                    .captures_iter(regex_cursor::Input::new(lines))
                    .map(|cap| {
                        let cap = lines.byte_slice(cap.get_group(1).unwrap().range());
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

        let node = query_match.matched_node(node_idx).node.clone();
        Some(InjectionQueryMatch {
            language,
            scope,
            include_children: properties.map(|p| p.include_children).unwrap_or_default(),
            last_match: last_content_node == node_idx,
            pattern: query_match.pattern(),
            extent: match_extent(&self.capture_depths, query_match),
            node,
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
    fn execute<'a, L: LanguageLoader>(
        &'a self,
        node: &Node<'a>,
        source: RopeSlice<'a>,
        loader: &'a L,
        range: Range,
    ) -> impl Iterator<Item = InjectionQueryMatch<'a>> + use<'a, L> {
        let mut cursor = InactiveQueryCursor::new(range, TREE_SITTER_MATCH_LIMIT).execute_query(
            &self.injection_query,
            node,
            source,
        );
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

    /// Runs the query over `spans`, after growing them with [`grow_spans`]. Returns the grown
    /// spans and the matches.
    fn execute_narrowed<'a, L: LanguageLoader>(
        &'a self,
        node: &Node<'a>,
        source: RopeSlice<'a>,
        loader: &'a L,
        old: &[Injection],
        mut spans: Vec<Range>,
    ) -> (Vec<Range>, Vec<InjectionQueryMatch<'a>>) {
        loop {
            let old_extents = old.iter().map(|injection| &injection.pattern_range);
            grow_spans(
                &self.injection_query,
                &self.capture_depths,
                node,
                source,
                old_extents,
                &mut spans,
            );
            // Each match lies inside a single span, so concatenating the results keeps them in order.
            let matches: Vec<_> = spans
                .iter()
                .flat_map(|span| self.execute(node, source, loader, span.clone()))
                .collect();
            // `execute` sees the same matches as the pre-pass, so this cannot grow the spans
            // unless the two cursors hit the match limit differently.
            let escaped = matches
                .iter()
                .fold(false, |grew, mat| grew | absorb(&mut spans, &mat.extent));
            if !escaped {
                return (spans, matches);
            }
        }
    }
}

/// The byte range of the node `query_match` is rooted at. `depths` holds each pattern's capture depths.
pub(crate) fn match_extent(depths: &[Vec<(Capture, u32)>], query_match: &QueryMatch) -> Range {
    let depths = &depths[query_match.pattern().idx()];
    let start = query_match
        .matched_nodes()
        .filter_map(|matched| {
            let (_, depth) = depths.iter().find(|(c, _)| *c == matched.capture)?;
            Some((&matched.node, *depth))
        })
        .min_by_key(|&(_, depth)| depth);
    let Some((node, mut depth)) = start else {
        return 0..u32::MAX;
    };
    let mut node = node.clone();
    while depth > 0 {
        let Some(parent) = node.parent() else {
            break;
        };
        node = parent;
        depth -= 1;
    }
    node.byte_range()
}

/// Grows `spans` until each one contains every new match and old result it touches. `old`
/// holds the old results' extents, mapped through the edits.
pub(crate) fn grow_spans<'a>(
    query: &Query,
    depths: &[Vec<(Capture, u32)>],
    node: &Node,
    source: RopeSlice,
    old: impl Iterator<Item = &'a Range> + Clone,
    spans: &mut Vec<Range>,
) {
    loop {
        while old
            .clone()
            .fold(false, |grew, extent| grew | absorb(spans, extent))
        {}
        // A cursor only reports captures inside its range, so a span misses a match that also
        // has captures outside it (e.g. renaming `htmx!(..)` to `html!(..)` puts only the name
        // in the span). Such a match is rooted at a node that crosses a span edge, so run
        // `next_match` over each edge byte, which reports those matches with all captures.
        let mut grew = false;
        let edges: Vec<_> = spans
            .iter()
            .flat_map(|span| [span.start..span.start + 1, span.end - 1..span.end])
            .collect();
        for edge in edges {
            let mut cursor = InactiveQueryCursor::new(edge, TREE_SITTER_MATCH_LIMIT)
                .execute_query(query, node, source);
            while let Some(query_match) = cursor.next_match() {
                // A match without captures (e.g. a disabled `local.reference`) produces nothing.
                if query_match.matched_nodes().next().is_some() {
                    grew |= absorb(spans, &match_extent(depths, &query_match));
                }
            }
        }
        if !grew {
            return;
        }
    }
}

impl Syntax {
    pub(crate) fn run_injection_query(
        &mut self,
        layer: Layer,
        map: &EditMap,
        source: RopeSlice<'_>,
        loader: &impl LanguageLoader,
        mut parse_layer: impl FnMut(Layer),
        spans: Option<Vec<Range>>,
    ) {
        self.map_injections(layer, map);
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

        if spans.as_ref().is_some_and(|spans| spans.is_empty()) {
            self.update_children(layer, &mut parse_layer);
            return;
        }

        // work around borrow checker
        let parent_ranges = take(&mut layer_data.ranges);
        let parse_tree = layer_data.parse_tree.take().unwrap();
        let all_old = take(&mut layer_data.injections);

        // Only injections that intersect the spans are re-queried. Without spans, the whole layer is re-queried.
        let spans = spans.unwrap_or_else(|| std::iter::once(0..u32::MAX).collect());
        let (spans, matches) = injections_query.execute_narrowed(
            &parse_tree.root_node(),
            source,
            loader,
            &all_old,
            spans,
        );
        // Old injections that intersect a span lie inside it and get replaced.
        let (old_in_span, retained): (Vec<_>, Vec<_>) = all_old
            .into_iter()
            .partition(|injection| intersects_any(&spans, &injection.range));
        // New matches of a combined pattern reuse the layer of its kept injections. Marking the
        // layer as reused stops other matches from taking it.
        let mut preseeded: HashMap<InjectionScope, Layer> = HashMap::new();
        for injection in &retained {
            if let Some(scope) = self.layer(injection.layer).combined_scope {
                preseeded.insert(scope, injection.layer);
                self.layer_mut(injection.layer).flags.reused = true;
            }
        }

        let mut injections: Vec<Injection> = Vec::with_capacity(old_in_span.len());
        let mut old_injections = old_in_span.into_iter().peekable();

        let mut combined_injections: HashMap<InjectionScope, Layer> = HashMap::with_capacity(32);
        for mat in matches {
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
            if injections
                .last()
                .is_some_and(|injection| injection.range.end > matched_node_range.start)
            {
                insert_position = injections
                    .partition_point(|injection| injection.range.end <= matched_node_range.start);
                if injections[insert_position].range.start < matched_node_range.end {
                    continue;
                }
            }

            // Combined injection matches share one layer and may still reuse that layer's old
            // injections.
            let combined_layer = mat.scope.and_then(|scope| {
                combined_injections
                    .get(&scope)
                    .or_else(|| preseeded.get(&scope))
                    .copied()
            });
            let reused_injection = self.reuse_injection(
                mat.language,
                &matched_node_range,
                combined_layer,
                &mut old_injections,
            );
            // Only `injection.combined` layers keep their scope after the update. A
            // multi-content-node match is always re-run whole, so its layer doesn't need one.
            let combined_scope = mat
                .scope
                .filter(|scope| matches!(scope, InjectionScope::Pattern { .. }));
            let layer = match mat.scope {
                Some(scope @ InjectionScope::Match { .. }) if mat.last_match => {
                    combined_injections.remove(&scope).unwrap_or_else(|| {
                        self.init_injection(layer, mat.language, reused_injection, combined_scope)
                    })
                }
                Some(scope) => match combined_injections.get(&scope) {
                    Some(&combined) => combined,
                    None => {
                        // Bind to the layer of the pattern's retained injections, if any.
                        let injection_layer = match preseeded.remove(&scope) {
                            Some(existing) => existing,
                            None => self.init_injection(
                                layer,
                                mat.language,
                                reused_injection,
                                combined_scope,
                            ),
                        };
                        combined_injections.insert(scope, injection_layer);
                        injection_layer
                    }
                },
                None => self.init_injection(layer, mat.language, reused_injection, combined_scope),
            };

            let old_len = injections.len();
            for range in intersect_ranges(mat.include_children, mat.node, &parent_ranges) {
                injections.push(Injection {
                    range,
                    layer,
                    pattern_range: mat.extent.clone(),
                });
            }
            if old_len != insert_position {
                let inserted = injections.len() - old_len;
                injections[insert_position..].rotate_right(inserted);
            }
        }

        // Merge the kept injections with the re-queried ones.
        let mut final_injections = retained;
        final_injections.append(&mut injections);
        final_injections.sort_by_key(|injection| injection.range.start);

        debug_assert!(
            final_injections
                .windows(2)
                .all(|pair| pair[0].range.end <= pair[1].range.start),
            "injections must stay sorted and non-overlapping after splicing: {final_injections:?}"
        );
        let layer_data = &mut self.layer_mut(layer);
        layer_data.ranges = parent_ranges;
        layer_data.parse_tree = Some(parse_tree);
        layer_data.injections = final_injections;
        self.update_children(layer, &mut parse_layer);
    }

    /// Rebuilds the included ranges of the layer's children from its injections and queues
    /// children that haven't been visited yet.
    fn update_children(&mut self, layer: Layer, parse_layer: &mut impl FnMut(Layer)) {
        let injections = take(&mut self.layer_mut(layer).injections);
        for injection in &injections {
            self.layer_mut(injection.layer).ranges.clear();
        }
        for injection in &injections {
            let child = self.layer_mut(injection.layer);
            child.ranges.push(tree_sitter::Range::new(
                tree_sitter::Point::ZERO,
                tree_sitter::Point::ZERO,
                injection.range.start,
                injection.range.end,
            ));
            if !child.flags.touched {
                child.flags.touched = true;
                parse_layer(injection.layer);
            }
        }
        self.layer_mut(layer).injections = injections;
    }

    /// Maps the layer's injections through the edits. Injections no edit touched just shift.
    /// The others are approximate until the next parse re-queries them.
    pub(crate) fn map_injections(&mut self, layer: Layer, map: &EditMap) {
        let injections = &self.layer(layer).injections;
        let end = injections
            .iter()
            .map(|injection| injection.pattern_range.end)
            .max();
        if map
            .edits
            .first()
            .is_none_or(|edit| end.is_none_or(|end| end < edit.start_byte))
        {
            return;
        }
        for injection in &mut self.layer_mut(layer).injections {
            injection.range = map.down(injection.range.start)..map.up(injection.range.end);
            injection.pattern_range =
                map.down(injection.pattern_range.start)..map.up(injection.pattern_range.end);
        }
    }

    fn init_injection(
        &mut self,
        parent: Layer,
        language: Language,
        reuse: Option<Injection>,
        combined_scope: Option<InjectionScope>,
    ) -> Layer {
        match reuse {
            Some(old_injection) => {
                let layer_data = self.layer_mut(old_injection.layer);
                debug_assert_eq!(layer_data.parent, Some(parent));
                layer_data.flags.reused = true;
                layer_data.combined_scope = combined_scope;
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
                    combined_scope,
                });
                Layer(layer as u32)
            }
        }
    }

    /// Picks the old injection whose layer the new match inherits. A bad pick only costs a
    /// fresh parse.
    fn reuse_injection(
        &mut self,
        language: Language,
        new_range: &Range,
        // The layer already chosen for this match's combined scope, if any.
        combined_layer: Option<Layer>,
        injections: &mut Peekable<impl Iterator<Item = Injection>>,
    ) -> Option<Injection> {
        while injections
            .next_if(|injection| injection.range.end <= new_range.start)
            .is_some()
        {}
        injections.next_if(|injection| {
            injection.range.start < new_range.end
                && self.layer(injection.layer).language == language
                && (!self.layer(injection.layer).flags.reused
                    || combined_layer == Some(injection.layer))
        })
    }
}

fn intersect_ranges<'tree, 'p>(
    include_children: IncludedChildren,
    node: Node<'tree>,
    parent_ranges: &'p [tree_sitter::Range],
) -> impl Iterator<Item = Range> + use<'tree, 'p> {
    let range = node.byte_range();
    let i = parent_ranges.partition_point(|parent_range| parent_range.end_byte <= range.start);
    let parent_ranges = parent_ranges[i..]
        .iter()
        .map(|range| range.start_byte..range.end_byte);
    let excluded_ranges = node
        .children()
        .filter(move |node| match include_children {
            IncludedChildren::None => true,
            IncludedChildren::All => false,
            IncludedChildren::Unnamed => node.is_named(),
        })
        .map(|n| n.byte_range());

    intersect_ranges_impl(range, excluded_ranges, parent_ranges)
}

/// Creates an iterator over the subtraction of one set of ranges from another.
///
/// Given two iterators that yield a sorted series of ranges, this function
/// returns a new iterator that yields a series of ranges, equivalent to the
/// ranges from the first iterator, but excluding any ranges returned by the
/// second.
///
/// To reiterate, both iterators are assumed to return the ranges in sorted order.
fn exclude_ranges(
    mut ranges: impl Iterator<Item = Range>,
    excluded_ranges: impl Iterator<Item = Range>,
) -> impl Iterator<Item = Range> {
    let mut excluded_ranges = excluded_ranges.filter(|range| !range.is_empty());
    let mut next_range = ranges.next();
    let mut current_excluded_range = excluded_ranges.next();
    from_fn(move || {
        loop {
            let range = next_range.take()?;

            // Consume any ranges that precede the rest of the input ranges
            while let Some(excluded_range) = &current_excluded_range {
                if excluded_range.end <= range.start {
                    current_excluded_range = excluded_ranges.next()
                } else {
                    break;
                }
            }
            // Handle the next exclusion, if applicable
            if let Some(excluded_range) = &current_excluded_range {
                if ranges_intersect(&range, excluded_range) {
                    let preceding_range = range.start..excluded_range.start;
                    let remaining_range = excluded_range.end..range.end;

                    // Handle the rest of the range on the next iteration, if
                    // applicable, or take the next one.
                    if !remaining_range.is_empty() {
                        next_range = Some(remaining_range);
                        // This would also be handled by the loop above
                        current_excluded_range = excluded_ranges.next()
                    } else {
                        next_range = ranges.next()
                    }

                    // Return any part of the range that preceded the exclusion
                    if !preceding_range.is_empty() {
                        return Some(preceding_range);
                    } else {
                        continue;
                    }
                }
            }
            // Return any ranges that precede the next excluded range
            next_range = ranges.next();
            return Some(range);
        }
    })
}

/// Return an iterator over the ranges resulting from intersection calculation.
///
/// Given a range, a set of ranges to exclude, and a set of parent ranges,
/// this function calculates the intersection of range with the parent ranges,
/// minus the excluded ranges.
fn intersect_ranges_impl(
    mut range: Range,
    excluded_ranges: impl Iterator<Item = Range>,
    parent_ranges: impl Iterator<Item = Range>,
) -> impl Iterator<Item = Range> {
    // Skip ranges that end before the start of range
    let parent_ranges = parent_ranges.skip_while(move |r| r.end <= range.start);
    // filter by the excluded ranges
    let mut parent_ranges = exclude_ranges(parent_ranges, excluded_ranges);
    let mut next_parent_range = parent_ranges.next();
    from_fn(move || {
        loop {
            // If either range is exhausted, so is this iterator
            let Some(parent_range) = &next_parent_range else {
                return None;
            };
            if range.is_empty() {
                return None;
            }

            // Discard non-intersecting part of range
            if parent_range.start > range.start {
                range.start = parent_range.start;
            }

            // Consume some or all of the range. Adjacent parent ranges are not merged: they
            // belong to different injections of the parent layer.
            let intersection = range.start..std::cmp::min(range.end, parent_range.end);
            range.start = intersection.end;

            // Discard any parents that fully precede the new range
            while next_parent_range
                .as_ref()
                .is_some_and(|parent_range| parent_range.end <= range.start)
            {
                next_parent_range = parent_ranges.next();
            }

            // Return the part of the range that was consumed
            if !intersection.is_empty() {
                return Some(intersection);
            }
        }
    })
}

/// Maps old positions through `edits`. A position inside an edit snaps to the start (`down`)
/// or end (`up`) of its replacement, so mapped ranges never shrink.
pub(crate) struct EditMap<'a> {
    edits: &'a [tree_sitter::InputEdit],
    /// `offsets[i]` is the total size change of the edits before edit `i`. The last entry
    /// covers all edits.
    offsets: Vec<i32>,
}

impl<'a> EditMap<'a> {
    pub(crate) fn new(edits: &'a [tree_sitter::InputEdit]) -> Self {
        let offsets = iter::once(0)
            .chain(edits.iter().scan(0, |offset, edit| {
                *offset += edit.offset();
                Some(*offset)
            }))
            .collect();
        Self { edits, offsets }
    }

    fn map(&self, pos: u32, up: bool) -> u32 {
        let i = self.edits.partition_point(|edit| edit.old_end_byte < pos);
        let pos = match self.edits.get(i) {
            Some(edit) if edit.start_byte <= pos && up => edit.new_end_byte,
            Some(edit) if edit.start_byte <= pos => edit.start_byte,
            _ => pos,
        };
        (pos as i32 + self.offsets[i]) as u32
    }

    pub(crate) fn down(&self, pos: u32) -> u32 {
        self.map(pos, false)
    }

    pub(crate) fn up(&self, pos: u32) -> u32 {
        self.map(pos, true)
    }

    /// The text each edit inserted, in new coordinates.
    pub(crate) fn edited_regions(&self) -> Vec<Range> {
        self.edits
            .iter()
            .zip(&self.offsets)
            .map(|(edit, offset)| {
                (edit.start_byte as i32 + offset) as u32..(edit.new_end_byte as i32 + offset) as u32
            })
            .collect()
    }
}

/// Sorts `spans` and merges those that overlap or touch, dropping empty ones.
pub(crate) fn merge_spans(mut spans: Vec<Range>) -> Vec<Range> {
    spans.retain(|span| !span.is_empty());
    spans.sort_unstable_by_key(|span| span.start);
    spans.dedup_by(|next, prev| {
        let merge = next.start <= prev.end;
        if merge {
            prev.end = prev.end.max(next.end);
        }
        merge
    });
    spans
}

/// Grows the spans `range` intersects to cover it, merging any that then touch. Returns
/// whether a span grew.
pub(crate) fn absorb(spans: &mut Vec<Range>, range: &Range) -> bool {
    let mut lo = spans.partition_point(|span| span.end <= range.start);
    if !spans
        .get(lo)
        .is_some_and(|span| ranges_intersect(span, range))
    {
        return false;
    }
    let mut hi = lo + 1;
    let mut merged = spans[lo].start.min(range.start)..range.end;
    while lo > 0 && spans[lo - 1].end >= merged.start {
        lo -= 1;
        merged.start = spans[lo].start;
    }
    while hi < spans.len() && spans[hi].start <= merged.end {
        hi += 1;
    }
    merged.end = merged.end.max(spans[hi - 1].end);
    let grew = hi - lo > 1 || merged != spans[lo];
    spans.splice(lo..hi, [merged]);
    grew
}

/// Whether `range` intersects any of the sorted, non-overlapping `spans`.
pub(crate) fn intersects_any(spans: &[Range], range: &Range) -> bool {
    let i = spans.partition_point(|span| span.end <= range.start);
    spans
        .get(i)
        .is_some_and(|span| ranges_intersect(span, range))
}

fn ranges_intersect(a: &Range, b: &Range) -> bool {
    // Adapted from <https://github.com/helix-editor/helix/blob/8df58b2e1779dcf0046fb51ae1893c1eebf01e7c/helix-core/src/selection.rs#L156-L163>
    a.start == b.start || (a.end > b.start && b.end > a.start)
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    #[test]
    fn exclude_ranges_no_exclusions() {
        let result: Vec<Range> =
            exclude_ranges([0..10, 20..30].into_iter(), [].into_iter()).collect();
        assert_eq!(result, vec![0..10, 20..30]);
    }

    #[test]
    fn exclude_ranges_excludes_prefix() {
        let result: Vec<Range> = exclude_ranges([0..10].into_iter(), [0..5].into_iter()).collect();
        assert_eq!(result, vec![5..10]);
    }

    #[test]
    fn exclude_ranges_excludes_suffix() {
        let result: Vec<Range> = exclude_ranges([0..10].into_iter(), [5..10].into_iter()).collect();
        assert_eq!(result, vec![0..5]);
    }

    #[test]
    fn exclude_ranges_excludes_middle() {
        let result: Vec<Range> = exclude_ranges([0..10].into_iter(), [3..7].into_iter()).collect();
        assert_eq!(result, vec![0..3, 7..10]);
    }

    #[test]
    fn exclude_ranges_full_exclusion() {
        let result: Vec<Range> = exclude_ranges([0..10].into_iter(), [0..10].into_iter()).collect();
        assert_eq!(result, vec![]);
    }

    #[test]
    fn exclude_ranges_multiple_exclusions() {
        let result: Vec<Range> =
            exclude_ranges([0..100].into_iter(), [10..20, 40..50, 70..80].into_iter()).collect();
        assert_eq!(result, vec![0..10, 20..40, 50..70, 80..100]);
    }

    #[test]
    fn exclude_ranges_spans_multiple_input_ranges() {
        let result: Vec<Range> =
            exclude_ranges([0..20, 30..50].into_iter(), [15..35].into_iter()).collect();
        assert_eq!(result, vec![0..15, 35..50]);
    }

    #[test]
    fn intersect_ranges_impl_basic() {
        let result: Vec<Range> =
            intersect_ranges_impl(10..50, [].into_iter(), [0..100].into_iter()).collect();
        assert_eq!(result, vec![10..50]);
    }

    #[test]
    fn intersect_ranges_impl_with_excluded_child() {
        let result: Vec<Range> =
            intersect_ranges_impl(0..100, [30..70].into_iter(), [0..100].into_iter()).collect();
        assert_eq!(result, vec![0..30, 70..100]);
    }

    #[test]
    fn intersect_ranges_impl_non_adjacent_parent_ranges() {
        let result: Vec<Range> =
            intersect_ranges_impl(0..100, [].into_iter(), [10..40, 60..90].into_iter()).collect();
        assert_eq!(result, vec![10..40, 60..90]);
    }

    #[test]
    fn intersect_ranges_impl_adjacent_parent_ranges() {
        // Adjacent parent ranges belong to different injections of the parent layer, so a child crossing the seam is split.
        let result: Vec<Range> =
            intersect_ranges_impl(0..100, [].into_iter(), [10..50, 50..80].into_iter()).collect();
        assert_eq!(result, vec![10..50, 50..80]);
        let result: Vec<Range> =
            intersect_ranges_impl(20..70, [].into_iter(), [10..30, 30..50, 50..80].into_iter())
                .collect();
        assert_eq!(result, vec![20..30, 30..50, 50..70]);
    }
}
