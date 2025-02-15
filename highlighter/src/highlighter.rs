use std::borrow::Cow;
use std::mem::replace;
use std::ops::RangeBounds;
use std::path::Path;
use std::slice;
use std::sync::Arc;

use crate::config::{LanguageConfig, LanguageLoader};
use crate::locals::ScopeCursor;
use crate::query_iter::{MatchedNode, QueryIter, QueryIterEvent, QueryLoader};
use crate::{Injection, Language, Layer, Syntax};
use arc_swap::ArcSwap;
use hashbrown::HashSet;
use ropey::RopeSlice;
use tree_sitter::Pattern;
use tree_sitter::{
    query::{self, Query, UserPredicate},
    Capture, Grammar,
};

/// Contains the data needed to highlight code written in a particular language.
///
/// This struct is immutable and can be shared between threads.
#[derive(Debug)]
pub struct HighlightQuery {
    pub query: Query,
    highlight_indices: ArcSwap<Vec<Highlight>>,
    #[allow(dead_code)]
    /// Patterns that do not match when the node is a local.
    non_local_patterns: HashSet<Pattern>,
    local_reference_capture: Option<Capture>,
}

impl HighlightQuery {
    pub(crate) fn new(
        grammar: Grammar,
        highlight_query_text: &str,
        highlight_query_path: impl AsRef<Path>,
        local_query_text: &str,
    ) -> Result<Self, query::ParseError> {
        // Concatenate the highlights and locals queries.
        let mut query_source =
            String::with_capacity(highlight_query_text.len() + local_query_text.len());
        query_source.push_str(highlight_query_text);
        query_source.push_str(local_query_text);

        let mut non_local_patterns = HashSet::new();
        let mut query = Query::new(
            grammar,
            &query_source,
            highlight_query_path,
            |pattern, predicate| {
                match predicate {
                    // Allow the `(#set! local.scope-inherits <bool>)` property to be parsed.
                    // This information is not used by this query though, it's used in the
                    // injection query instead.
                    UserPredicate::SetProperty {
                        key: "local.scope-inherits",
                        ..
                    } => (),
                    // TODO: `(#is(-not)? local)` applies to the entire pattern. Ideally you
                    // should be able to supply capture(s?) which are each checked.
                    UserPredicate::IsPropertySet {
                        negate: true,
                        key: "local",
                        val: None,
                    } => {
                        non_local_patterns.insert(pattern);
                    }
                    _ => return Err(format!("unsupported predicate {predicate}").into()),
                }
                Ok(())
            },
        )?;

        // The highlight query only cares about local.reference captures. All scope and definition
        // captures can be disabled.
        query.disable_capture("local.scope");
        let local_definition_captures: Vec<_> = query
            .captures()
            .filter(|&(_, name)| name.starts_with("local.definition."))
            .map(|(_, name)| Box::<str>::from(name))
            .collect();
        for name in local_definition_captures {
            query.disable_capture(&name);
        }

        Ok(Self {
            highlight_indices: ArcSwap::from_pointee(vec![
                Highlight::NONE;
                query.num_captures() as usize
            ]),
            non_local_patterns,
            local_reference_capture: query.get_capture("local.reference"),
            query,
        })
    }

    /// Configures the list of recognized highlight names.
    ///
    /// Tree-sitter syntax-highlighting queries specify highlights in the form of dot-separated
    /// highlight names like `punctuation.bracket` and `function.method.builtin`. Consumers of
    /// these queries can choose to recognize highlights with different levels of specificity.
    /// For example, the string `function.builtin` will match against `function.builtin.constructor`
    /// but will not match `function.method.builtin` and `function.method`.
    ///
    /// The closure provided to this function should therefore try to first lookup the full
    /// name. If no highlight was found for that name it should [`rsplit_once('.')`](str::rsplit_once)
    /// and retry until a highlight has been found. If none of the parent scopes are defined
    /// then `Highlight::NONE` should be returned.
    ///
    /// When highlighting, results are returned as `Highlight` values, configured by this function.
    /// The meaning of these indices is up to the user of the implementation. The highlighter
    /// treats the indices as entirely opaque.
    pub(crate) fn configure(&self, f: &mut impl FnMut(&str) -> Highlight) {
        let highlight_indices = self
            .query
            .captures()
            .map(|(_, capture_name)| f(capture_name))
            .collect();
        self.highlight_indices.store(Arc::new(highlight_indices));
    }
}

/// Indicates which highlight should be applied to a region of source code.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Highlight(pub u32);

impl Highlight {
    pub const NONE: Highlight = Highlight(u32::MAX);
}

#[derive(Debug)]
struct HighlightedNode {
    end: u32,
    highlight: Highlight,
}

#[derive(Debug)]
pub struct LayerData<'tree> {
    parent_highlights: usize,
    dormant_highlights: Vec<HighlightedNode>,
    scope_cursor: ScopeCursor<'tree>,
}

impl<'tree> LayerData<'tree> {
    fn new(syntax: &'tree Syntax, injection: &Injection) -> Self {
        let scope_cursor = syntax.layer(injection.layer).locals.scope_cursor(0);

        Self {
            parent_highlights: Default::default(),
            dormant_highlights: Default::default(),
            scope_cursor,
        }
    }
}

pub struct Highlighter<'a, 'tree, Loader: LanguageLoader> {
    query: QueryIter<'a, 'tree, HighlightQueryLoader<&'a Loader>, LayerData<'tree>>,
    next_query_event: Option<QueryIterEvent<'tree, LayerData<'tree>>>,
    active_highlights: Vec<HighlightedNode>,
    next_highlight_end: u32,
    next_highlight_start: u32,
    active_config: Option<&'a LanguageConfig>,
    /// The current injection layer of the query iterator.
    ///
    /// We track this in the highlighter (rather than calling `QueryIter::current_layer`) because
    /// the highlighter peeks events from the QueryIter (see `Self::advance_query_iter`).
    current_layer: Layer,
}

pub struct HighlightList<'a>(slice::Iter<'a, HighlightedNode>);

impl Iterator for HighlightList<'_> {
    type Item = Highlight;

    fn next(&mut self) -> Option<Highlight> {
        self.0.next().map(|node| node.highlight)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl DoubleEndedIterator for HighlightList<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back().map(|node| node.highlight)
    }
}

pub enum HighlightEvent<'a> {
    RefreshHighlights(HighlightList<'a>),
    PushHighlights(HighlightList<'a>),
}

impl<'a, 'tree: 'a, Loader: LanguageLoader> Highlighter<'a, 'tree, Loader> {
    pub fn new(
        syntax: &'tree Syntax,
        src: RopeSlice<'a>,
        loader: &'a Loader,
        range: impl RangeBounds<u32>,
    ) -> Self {
        let mut query = QueryIter::new(
            syntax,
            src,
            HighlightQueryLoader(loader),
            LayerData::new,
            range,
        );
        let active_language = query.current_language();
        let mut res = Highlighter {
            active_config: query.loader().0.get_config(active_language),
            next_query_event: None,
            current_layer: query.current_layer(),
            active_highlights: Vec::new(),
            next_highlight_end: u32::MAX,
            next_highlight_start: 0,
            query,
        };
        res.advance_query_iter();
        res
    }

    pub fn active_highlights(&self) -> HighlightList<'_> {
        HighlightList(self.active_highlights.iter())
    }

    pub fn next_event_offset(&self) -> u32 {
        self.next_highlight_start.min(self.next_highlight_end)
    }

    pub fn advance(&mut self) -> HighlightEvent<'_> {
        let mut refresh = false;
        let prev_stack_size = self.active_highlights.len();

        let pos = self.next_event_offset();
        if self.next_highlight_end == pos {
            // self.process_injection_ends();
            self.process_highlight_end(pos);
            refresh = true;
        }

        let mut first_highlight = true;
        while self.next_highlight_start == pos {
            let Some(query_event) = self.advance_query_iter() else {
                break;
            };
            match query_event {
                QueryIterEvent::EnterInjection(_) => self.enter_injection(),
                QueryIterEvent::Match(node) => self.start_highlight(node, &mut first_highlight),
                QueryIterEvent::ExitInjection { injection, state } => {
                    // state is returned if the layer is finished, if it isn't we have
                    // a combined injection and need to deactivate its highlights
                    if state.is_none() {
                        self.deactivate_layer(injection.layer);
                        refresh = true;
                    }
                    let active_language = self.query.current_language();
                    self.active_config = self.query.loader().0.get_config(active_language);
                }
            }
        }
        self.next_highlight_end = self
            .active_highlights
            .last()
            .map_or(u32::MAX, |node| node.end);

        if refresh {
            HighlightEvent::RefreshHighlights(HighlightList(self.active_highlights.iter()))
        } else {
            HighlightEvent::PushHighlights(HighlightList(
                self.active_highlights[prev_stack_size..].iter(),
            ))
        }
    }

    fn advance_query_iter(&mut self) -> Option<QueryIterEvent<'tree, LayerData>> {
        // Track the current layer **before** calling `QueryIter::next`. The QueryIter moves
        // to the next event with `QueryIter::next` but we're treating that event as peeked - it
        // hasn't occurred yet - so the current layer is the one the query iter was on _before_
        // `QueryIter::next`.
        self.current_layer = self.query.current_layer();
        let event = replace(&mut self.next_query_event, self.query.next());
        self.next_highlight_start = self
            .next_query_event
            .as_ref()
            .map_or(u32::MAX, |event| event.start_byte());
        event
    }

    fn process_highlight_end(&mut self, pos: u32) {
        let i = self
            .active_highlights
            .iter()
            .rposition(|highlight| highlight.end != pos)
            .map_or(0, |i| i + 1);
        self.active_highlights.truncate(i);
    }

    fn enter_injection(&mut self) {
        let active_language = self.query.syntax().layer(self.current_layer).language;
        self.active_config = self.query.loader().0.get_config(active_language);
        let data = self.query.current_injection().1;
        data.parent_highlights = self.active_highlights.len();
        self.active_highlights.append(&mut data.dormant_highlights);
    }

    fn deactivate_layer(&mut self, layer: Layer) {
        let LayerData {
            mut parent_highlights,
            ref mut dormant_highlights,
            ..
        } = *self.query.layer_state(layer);
        parent_highlights = parent_highlights.min(self.active_highlights.len());
        dormant_highlights.extend(self.active_highlights.drain(parent_highlights..));
        self.process_highlight_end(self.next_highlight_start);
    }

    fn start_highlight(&mut self, node: MatchedNode, first_highlight: &mut bool) {
        let range = node.node.byte_range();
        // `<QueryIter as Iterator>::next` skips matches with empty ranges.
        debug_assert!(
            !range.is_empty(),
            "QueryIter should not emit matches with empty ranges"
        );

        let config = self
            .active_config
            .expect("must have an active config to emit matches");

        let highlight = if Some(node.capture) == config.highlight_query.local_reference_capture {
            // If this capture was a `@local.reference` from the locals queries, look up the
            // text of the node in the current locals cursor and use that highlight.
            let text: Cow<str> = self
                .query
                .source()
                .byte_slice(range.start as usize..range.end as usize)
                .into();
            let scope_cursor = &mut self.query.layer_state(self.current_layer).scope_cursor;
            let scope = scope_cursor.advance(range.start);
            let Some(capture) = scope_cursor.locals.lookup_reference(scope, &text) else {
                return;
            };
            config
                .injection_query
                .local_definition_captures
                .load()
                .get(&capture)
                .copied()
                .unwrap_or(Highlight::NONE)
        } else {
            // If the pattern is marked with `(#is-not? local)` and the matched node is a
            // reference to a local, discard this match.
            if config
                .highlight_query
                .non_local_patterns
                .contains(&node.pattern)
            {
                let text: Cow<str> = self
                    .query
                    .source()
                    .byte_slice(range.start as usize..range.end as usize)
                    .into();
                let scope_cursor = &mut self.query.layer_state(self.current_layer).scope_cursor;
                let scope = scope_cursor.advance(range.start);
                if scope_cursor.locals.lookup_reference(scope, &text).is_some() {
                    return;
                };
            }

            config.highlight_query.highlight_indices.load()[node.capture.idx()]
        };

        // If multiple patterns match this exact node, prefer the last one which matched.
        // This matches the precedence of Neovim, Zed, and tree-sitter-cli.
        if !*first_highlight
            && self
                .active_highlights
                .last()
                .is_some_and(|prev_node| prev_node.end == range.end)
        {
            self.active_highlights.pop();
        }
        if highlight != Highlight::NONE {
            self.active_highlights.push(HighlightedNode {
                end: range.end,
                highlight,
            });
            *first_highlight = false;
        }
    }
}

pub(crate) struct HighlightQueryLoader<T>(T);

impl<'a, T: LanguageLoader> QueryLoader<'a> for HighlightQueryLoader<&'a T> {
    fn get_query(&mut self, lang: Language) -> Option<&'a Query> {
        self.0
            .get_config(lang)
            .map(|config| &config.highlight_query.query)
    }
}
