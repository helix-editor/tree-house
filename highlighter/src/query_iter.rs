use core::slice;
use std::iter::Peekable;
use std::mem::replace;
use std::ops::RangeBounds;

use hashbrown::HashMap;
use ropey::RopeSlice;

use crate::{
    locals::{Scope, ScopeCursor},
    Injection, Language, Layer, Range, Syntax, TREE_SITTER_MATCH_LIMIT,
};
use tree_sitter::{
    Capture, InactiveQueryCursor, Node, Pattern, Query, QueryCursor, QueryMatch, RopeInput,
};

#[derive(Debug, Clone)]
pub struct MatchedNode<'tree> {
    pub match_id: u32,
    pub pattern: Pattern,
    pub node: Node<'tree>,
    pub capture: Capture,
    pub scope: Scope,
}

struct LayerQueryIter<'a, 'tree> {
    cursor: Option<QueryCursor<'a, 'tree, RopeInput<'a>>>,
    peeked: Option<MatchedNode<'tree>>,
    language: Language,
    scope_cursor: ScopeCursor<'tree>,
}

impl<'a, 'tree> LayerQueryIter<'a, 'tree> {
    fn peek<Loader: QueryLoader<'a>>(
        &mut self,
        source: RopeSlice<'_>,
        loader: &Loader,
    ) -> Option<&MatchedNode<'tree>> {
        if self.peeked.is_none() {
            loop {
                let (query_match, node_idx) = self.cursor.as_mut()?.next_matched_node()?;
                let node = query_match.matched_node(node_idx);
                let match_id = query_match.id();
                let pattern = query_match.pattern();
                let range = node.node.byte_range();
                let scope = self.scope_cursor.advance(range.start);

                if !loader.are_predicates_satisfied(
                    self.language,
                    &query_match,
                    source,
                    &self.scope_cursor,
                ) {
                    query_match.remove();
                    continue;
                }

                self.peeked = Some(MatchedNode {
                    match_id,
                    pattern,
                    // NOTE: `Node` is cheap to clone, it's essentially Copy.
                    node: node.node.clone(),
                    capture: node.capture,
                    scope,
                });
                break;
            }
        }
        self.peeked.as_ref()
    }

    fn consume(&mut self) -> MatchedNode<'tree> {
        self.peeked.take().unwrap()
    }
}

struct ActiveLayer<'a, 'tree, S> {
    state: S,
    query_iter: LayerQueryIter<'a, 'tree>,
    injections: Peekable<slice::Iter<'a, Injection>>,
}

// data only needed when entering and exiting injections
// separate struck to keep the QueryIter reasonably small
struct QueryIterLayerManager<'a, 'tree, Loader, S> {
    range: Range,
    loader: Loader,
    src: RopeSlice<'a>,
    syntax: &'tree Syntax,
    active_layers: HashMap<Layer, Box<ActiveLayer<'a, 'tree, S>>>,
    active_injections: Vec<Injection>,
}

impl<'a, 'tree: 'a, Loader, S> QueryIterLayerManager<'a, 'tree, Loader, S>
where
    Loader: QueryLoader<'a>,
    S: Default,
{
    fn init_layer(&mut self, injection: Injection) -> Box<ActiveLayer<'a, 'tree, S>> {
        self.active_layers
            .remove(&injection.layer)
            .unwrap_or_else(|| {
                let layer = self.syntax.layer(injection.layer);
                let start_point = injection.range.start.max(self.range.start);
                let injection_start = layer
                    .injections
                    .partition_point(|child| child.range.end < start_point);
                let cursor = self
                    .loader
                    .get_query(layer.language)
                    .and_then(|query| Some((query, layer.tree()?.root_node())))
                    .map(|(query, node)| {
                        let mut cursor = InactiveQueryCursor::new();
                        cursor.set_match_limit(TREE_SITTER_MATCH_LIMIT);
                        cursor.set_byte_range(self.range.clone());
                        cursor.execute_query(query, &node, RopeInput::new(self.src))
                    });
                Box::new(ActiveLayer {
                    state: S::default(),
                    query_iter: LayerQueryIter {
                        language: layer.language,
                        cursor,
                        peeked: None,
                        scope_cursor: layer.locals.scope_cursor(self.range.start),
                    },
                    injections: layer.injections[injection_start..].iter().peekable(),
                })
            })
    }
}

pub struct QueryIter<'a, 'tree, Loader: QueryLoader<'a>, LayerState = ()> {
    layer_manager: Box<QueryIterLayerManager<'a, 'tree, Loader, LayerState>>,
    current_layer: Box<ActiveLayer<'a, 'tree, LayerState>>,
    current_injection: Injection,
}

impl<'a, 'tree: 'a, Loader, LayerState> QueryIter<'a, 'tree, Loader, LayerState>
where
    Loader: QueryLoader<'a>,
    LayerState: Default,
{
    pub fn new(
        syntax: &'tree Syntax,
        src: RopeSlice<'a>,
        loader: Loader,
        range: impl RangeBounds<u32>,
    ) -> Self {
        let start = match range.start_bound() {
            std::ops::Bound::Included(&i) => i,
            std::ops::Bound::Excluded(&i) => i + 1,
            std::ops::Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            std::ops::Bound::Included(&i) => i + 1,
            std::ops::Bound::Excluded(&i) => i,
            std::ops::Bound::Unbounded => src.len_bytes() as u32,
        };
        let range = start..end;
        let node = syntax.tree().root_node();
        // create fake injection for query root
        let injection = Injection {
            range: node.byte_range(),
            layer: syntax.root,
            matched_node_range: node.byte_range(),
        };
        let mut layer_manager = Box::new(QueryIterLayerManager {
            range,
            loader,
            src,
            syntax,
            // TODO: reuse allocations with an allocation pool
            active_layers: HashMap::with_capacity(8),
            active_injections: Vec::with_capacity(8),
        });
        Self {
            current_layer: layer_manager.init_layer(injection.clone()),
            current_injection: injection,
            layer_manager,
        }
    }

    #[inline]
    pub fn source(&self) -> RopeSlice<'a> {
        self.layer_manager.src
    }

    #[inline]
    pub fn syntax(&self) -> &'tree Syntax {
        self.layer_manager.syntax
    }

    #[inline]
    pub fn loader(&mut self) -> &mut Loader {
        &mut self.layer_manager.loader
    }

    #[inline]
    pub fn current_layer(&self) -> Layer {
        self.current_injection.layer
    }

    #[inline]
    pub fn current_injection(&mut self) -> (Injection, &mut LayerState) {
        (
            self.current_injection.clone(),
            &mut self.current_layer.state,
        )
    }

    #[inline]
    pub fn current_language(&self) -> Language {
        self.layer_manager
            .syntax
            .layer(self.current_injection.layer)
            .language
    }

    pub fn layer_state(&mut self, layer: Layer) -> &mut LayerState {
        if layer == self.current_injection.layer {
            &mut self.current_layer.state
        } else {
            &mut self
                .layer_manager
                .active_layers
                .get_mut(&layer)
                .unwrap()
                .state
        }
    }

    fn enter_injection(&mut self, injection: Injection) {
        let active_layer = self.layer_manager.init_layer(injection.clone());
        let old_injection = replace(&mut self.current_injection, injection);
        let old_layer = replace(&mut self.current_layer, active_layer);
        self.layer_manager
            .active_layers
            .insert(old_injection.layer, old_layer);
        self.layer_manager.active_injections.push(old_injection);
    }

    fn exit_injection(&mut self) -> Option<(Injection, Option<LayerState>)> {
        let injection = replace(
            &mut self.current_injection,
            self.layer_manager.active_injections.pop()?,
        );
        let layer = replace(
            &mut self.current_layer,
            self.layer_manager
                .active_layers
                .remove(&self.current_injection.layer)?,
        );
        let layer_unfinished = layer.query_iter.peeked.is_some();
        if layer_unfinished {
            self.layer_manager
                .active_layers
                .insert(injection.layer, layer);
            Some((injection, None))
        } else {
            Some((injection, Some(layer.state)))
        }
    }
}

impl<'a, 'tree: 'a, Loader, S> Iterator for QueryIter<'a, 'tree, Loader, S>
where
    Loader: QueryLoader<'a>,
    S: Default,
{
    type Item = QueryIterEvent<'tree, S>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let next_injection = self
                .current_layer
                .injections
                .peek()
                .filter(|injection| injection.range.start <= self.current_injection.range.end);
            let next_match = self
                .current_layer
                .query_iter
                .peek(self.layer_manager.src, &self.layer_manager.loader)
                .filter(|matched_node| {
                    matched_node.node.start_byte() <= self.current_injection.range.end
                        && matched_node.node.start_byte() >= self.current_injection.range.start
                });

            match (next_match, next_injection) {
                (None, None) => {
                    return self.exit_injection().map(|(injection, state)| {
                        QueryIterEvent::ExitInjection { injection, state }
                    });
                }
                (Some(mat), _) if mat.node.byte_range().is_empty() => {
                    self.current_layer.query_iter.consume();
                    continue;
                }
                (Some(_), None) => {
                    // consume match
                    let matched_node = self.current_layer.query_iter.consume();
                    return Some(QueryIterEvent::Match(matched_node));
                }
                (Some(matched_node), Some(injection))
                    if matched_node.node.start_byte() < injection.range.end =>
                {
                    // consume match
                    let matched_node = self.current_layer.query_iter.consume();
                    // ignore nodes that are overlapped by the injection
                    if matched_node.node.start_byte() <= injection.range.start
                        || injection.range.end < matched_node.node.end_byte()
                    {
                        return Some(QueryIterEvent::Match(matched_node));
                    }
                }
                (Some(_), Some(_)) | (None, Some(_)) => {
                    // consume injection
                    let injection = self.current_layer.injections.next().unwrap();
                    self.enter_injection(injection.clone());
                    return Some(QueryIterEvent::EnterInjection(injection.clone()));
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum QueryIterEvent<'tree, State = ()> {
    EnterInjection(Injection),
    Match(MatchedNode<'tree>),
    ExitInjection {
        injection: Injection,
        state: Option<State>,
    },
}

impl<S> QueryIterEvent<'_, S> {
    pub fn start_byte(&self) -> u32 {
        match self {
            QueryIterEvent::EnterInjection(injection) => injection.range.start,
            QueryIterEvent::Match(mat) => mat.node.start_byte(),
            QueryIterEvent::ExitInjection { injection, .. } => injection.range.end,
        }
    }
}

pub trait QueryLoader<'a> {
    fn get_query(&mut self, lang: Language) -> Option<&'a Query>;

    fn are_predicates_satisfied(
        &self,
        _lang: Language,
        _match: &QueryMatch<'_, '_>,
        _source: RopeSlice<'_>,
        _locals_cursor: &ScopeCursor<'_>,
    ) -> bool {
        true
    }
}

impl<'a, F> QueryLoader<'a> for F
where
    F: FnMut(Language) -> Option<&'a Query>,
{
    fn get_query(&mut self, lang: Language) -> Option<&'a Query> {
        (self)(lang)
    }
}
