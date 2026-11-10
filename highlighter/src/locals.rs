use std::{
    borrow::Cow,
    ops::{Index, IndexMut},
    sync::Arc,
};

use hashbrown::HashMap;
use kstring::KString;
use ropey::RopeSlice;
use tree_sitter::{Capture, InactiveQueryCursor, Node, Pattern, Query};

use crate::highlighter::Highlight;
use crate::injections_query::{
    absorb, anchor_root, grow_spans, intersects_any, match_extent, EditMap,
};
use crate::{LanguageConfig, LanguageLoader, Layer, Range, Syntax, TREE_SITTER_MATCH_LIMIT};

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub struct Scope(u32);

impl Scope {
    const ROOT: Scope = Scope(0);
    fn idx(self) -> usize {
        self.0 as usize
    }
}

#[derive(Debug, Clone)]
pub struct Locals {
    scopes: Vec<ScopeData>,
    /// The captures `scopes` was built from, so a narrowed re-run can update them. `None` when
    /// they no longer match the tree.
    captures: Option<Vec<LocalCapture>>,
    /// Snapshot used to filter definitions when these scopes were built.
    configured_definitions: Option<Arc<HashMap<Capture, Highlight>>>,
}

#[derive(Debug, Clone)]
struct LocalCapture {
    range: Range,
    /// The range of the node the match is rooted at, see [`crate::Injection`].
    extent: Range,
    capture: Capture,
    pattern: Pattern,
}

impl Default for Locals {
    fn default() -> Self {
        let mut scopes = Vec::with_capacity(4);
        scopes.push(ScopeData {
            definitions: HashMap::new(),
            range: 0..u32::MAX,
            inherit: false,
            children: Vec::new(),
            parent: None,
        });

        Self {
            scopes,
            captures: None,
            configured_definitions: None,
        }
    }
}

impl Locals {
    pub(crate) fn configuration_changed(&self, config: &LanguageConfig) -> bool {
        let current = config.injection_query.local_definition_captures.load();
        self.configured_definitions
            .as_ref()
            .is_none_or(|previous| !Arc::ptr_eq(previous, &current))
    }

    fn push(&mut self, scope: ScopeData) -> Scope {
        let new_scope_id = Scope(self.scopes.len() as u32);
        let parent = scope
            .parent
            .expect("push cannot be used for the root layer");
        self[parent].children.push(new_scope_id);
        self.scopes.push(scope);
        new_scope_id
    }

    pub fn lookup_reference(&self, mut scope: Scope, name: &str) -> Option<&Definition> {
        loop {
            let scope_data = &self[scope];
            if let Some(def) = scope_data.definitions.get(name) {
                return Some(def);
            }
            if !scope_data.inherit {
                break;
            }
            scope = scope_data.parent?;
        }

        None
    }

    /// Maps all stored positions through `map`. If an edit touches the layer, the scopes are
    /// only approximate until the next parse rebuilds them.
    pub(crate) fn map(&mut self, map: &EditMap) {
        let map_pos = |pos: u32| map.down(pos);
        for scope in &mut self.scopes {
            // The root scope covers `0..u32::MAX` rather than real positions.
            if scope.range.end != u32::MAX {
                scope.range = map_pos(scope.range.start)..map_pos(scope.range.end);
            }
            for definition in scope.definitions.values_mut() {
                definition.range = map_pos(definition.range.start)..map_pos(definition.range.end);
            }
        }
        for capture in self.captures.iter_mut().flatten() {
            capture.range = map.down(capture.range.start)..map.up(capture.range.end);
            capture.extent = map.down(capture.extent.start)..map.up(capture.extent.end);
        }
    }

    pub fn scope_cursor(&self, pos: u32) -> ScopeCursor<'_> {
        let mut scope = Scope::ROOT;
        let mut scope_stack = Vec::with_capacity(8);
        loop {
            let scope_data = &self[scope];
            let child_idx = scope_data
                .children
                .partition_point(|&child| self[child].range.end < pos);
            scope_stack.push((scope, child_idx as u32));
            let Some(&child) = scope_data.children.get(child_idx) else {
                break;
            };
            if pos < self[child].range.start {
                break;
            }
            scope = child;
        }
        ScopeCursor {
            locals: self,
            scope_stack,
        }
    }
}

#[cfg(test)]
impl Locals {
    pub(crate) fn dump(&self) -> Vec<String> {
        let captures = self.captures.iter().flatten().map(|capture| {
            let LocalCapture {
                range,
                extent,
                capture,
                pattern,
            } = capture;
            format!("{range:?} {extent:?} {capture:?} {pattern:?}")
        });
        self.scopes
            .iter()
            .map(|scope| {
                let mut definitions: Vec<_> = scope
                    .definitions
                    .iter()
                    .map(|(name, def)| format!("{name}@{:?}:{:?}", def.range, def.capture))
                    .collect();
                definitions.sort();
                format!(
                    "{:?} inherit={} parent={:?} children={:?} {definitions:?}",
                    scope.range, scope.inherit, scope.parent, scope.children
                )
            })
            .chain(captures)
            .collect()
    }
}

impl Index<Scope> for Locals {
    type Output = ScopeData;

    fn index(&self, scope: Scope) -> &Self::Output {
        &self.scopes[scope.idx()]
    }
}

impl IndexMut<Scope> for Locals {
    fn index_mut(&mut self, scope: Scope) -> &mut Self::Output {
        &mut self.scopes[scope.idx()]
    }
}

#[derive(Debug)]
pub struct ScopeCursor<'a> {
    pub locals: &'a Locals,
    scope_stack: Vec<(Scope, u32)>,
}

impl ScopeCursor<'_> {
    pub fn advance(&mut self, to: u32) -> Scope {
        let (mut active_scope, mut child_idx) = self.scope_stack.pop().unwrap();
        loop {
            let scope_data = &self.locals[active_scope];
            if to < scope_data.range.end {
                break;
            }
            (active_scope, child_idx) = self.scope_stack.pop().unwrap();
            child_idx += 1;
        }
        'outer: loop {
            let scope_data = &self.locals[active_scope];
            loop {
                let Some(&child) = scope_data.children.get(child_idx as usize) else {
                    break 'outer;
                };
                if self.locals[child].range.start > to {
                    break 'outer;
                }
                if to < self.locals[child].range.end {
                    self.scope_stack.push((active_scope, child_idx));
                    active_scope = child;
                    child_idx = 0;
                    break;
                }
                child_idx += 1;
            }
        }
        self.scope_stack.push((active_scope, child_idx));
        active_scope
    }

    pub fn current_scope(&self) -> Scope {
        // The root scope is always active so `scope_stack` is never empty.
        self.scope_stack.last().unwrap().0
    }
}

#[derive(Debug, Clone)]
pub struct Definition {
    pub capture: Capture,
    pub range: Range,
}

#[derive(Debug, Clone)]
pub struct ScopeData {
    definitions: HashMap<KString, Definition>,
    range: Range,
    inherit: bool,
    /// A list of sorted, non-overlapping child scopes.
    ///
    /// See the docs of the `Locals` type: locals information is laid out like a tree - similar
    /// to injections - per injection layer.
    children: Vec<Scope>,
    parent: Option<Scope>,
}

impl Syntax {
    /// Rebuilds the layer's locals from its new tree. With `spans` (see
    /// [`Syntax::run_injection_query`]), only the captures intersecting them are re-queried.
    pub(crate) fn run_local_query(
        &mut self,
        layer: Layer,
        source: RopeSlice<'_>,
        loader: &impl LanguageLoader,
        spans: Option<Vec<Range>>,
    ) {
        let layer_data = &mut self.layer_mut(layer);
        let old = layer_data.locals.captures.take();
        let Some(LanguageConfig {
            ref injection_query,
            ..
        }) = loader.get_config(layer_data.language)
        else {
            return;
        };
        let definition_captures = injection_query.local_definition_captures.load_full();
        if definition_captures.is_empty() {
            layer_data.locals = Locals {
                configured_definitions: Some(definition_captures),
                ..Locals::default()
            };
            return;
        }

        let root = layer_data.parse_tree.as_ref().unwrap().root_node();
        let query = &injection_query.local_query;
        let depths = &injection_query.local_capture_depths;
        let captures = match (spans, old) {
            (Some(mut spans), Some(old)) => {
                let mut anchors = Vec::new();
                let new = loop {
                    let old_extents = old.iter().map(|capture| &capture.extent);
                    grow_spans(
                        query,
                        depths,
                        &root,
                        source,
                        old_extents,
                        &mut spans,
                        &mut anchors,
                    );
                    let mut new = Vec::new();
                    for span in &spans {
                        let span = span.clone();
                        query_captures(query, depths, &root, source, span, &anchors, &mut new);
                    }
                    let mut escaped = new.iter().fold(false, |grew, capture| {
                        grew | absorb(&mut spans, &capture.extent)
                    });
                    for anchor in &anchors {
                        let spread = injection_query.local_depth_spread;
                        if !anchor_holds(query, depths, spread, &root, source, anchor, &old, &new) {
                            absorb(&mut spans, anchor);
                            escaped = true;
                        }
                    }
                    if !escaped {
                        break new;
                    }
                };
                // Captures of the same node are either all kept or all re-queried, so the stable
                // sort by start keeps them in cursor order.
                let mut captures = old;
                captures.retain(|capture| {
                    !intersects_any(&spans, &capture.range) || anchors.contains(&capture.extent)
                });
                captures.extend(new);
                captures.sort_by_key(|capture| capture.range.start);
                captures
            }
            _ => {
                let mut captures = Vec::new();
                query_captures(
                    query,
                    depths,
                    &root,
                    source,
                    0..u32::MAX,
                    &[],
                    &mut captures,
                );
                captures
            }
        };

        let mut locals = Locals::default();
        let mut scope = Scope::ROOT;
        for local in &captures {
            let range = local.range.clone();
            let capture = local.capture;

            while range.start >= locals[scope].range.end {
                scope = locals[scope].parent.expect("root node covers entire range");
            }

            if Some(capture) == injection_query.local_scope_capture {
                scope = locals.push(ScopeData {
                    definitions: HashMap::new(),
                    range,
                    inherit: !injection_query.not_scope_inherits.contains(&local.pattern),
                    children: Vec::new(),
                    parent: Some(scope),
                });
            } else if definition_captures.contains_key(&capture) {
                let text = match source
                    .byte_slice(range.start as usize..range.end as usize)
                    .into()
                {
                    Cow::Borrowed(inner) => KString::from_ref(inner),
                    Cow::Owned(inner) => KString::from_string(inner),
                };
                locals[scope]
                    .definitions
                    .insert(text, Definition { capture, range });
            }
            // NOTE: `local.reference` captures are handled by the highlighter and are not
            // considered during parsing.
        }
        locals.captures = Some(captures);
        locals.configured_definitions = Some(definition_captures);

        layer_data.locals = locals;
    }
}

/// Appends the captures of the locals `query` intersecting `span`, in cursor order.
fn query_captures(
    query: &Query,
    depths: &[Vec<(Capture, u32)>],
    root: &Node,
    source: RopeSlice,
    span: Range,
    // Anchored extents (see `grow_spans`), whose captures are kept from the previous run.
    anchors: &[Range],
    out: &mut Vec<LocalCapture>,
) {
    // A cursor skips zero-width captures at the very start of its range so widen the range by a byte.
    // Such captures still count as intersecting the span.
    let range = span.start.saturating_sub(1)..span.end.saturating_add(1);
    let mut cursor =
        InactiveQueryCursor::new(range, TREE_SITTER_MATCH_LIMIT).execute_query(query, root, source);
    while let Some((query_match, node_idx)) = cursor.next_matched_node() {
        let matched_node = query_match.matched_node(node_idx);
        if !intersects_any(std::slice::from_ref(&span), &matched_node.node.byte_range()) {
            continue;
        }
        let extent = match_extent(depths, &query_match);
        if anchors.contains(&extent) {
            continue;
        }
        out.push(LocalCapture {
            range: matched_node.node.byte_range(),
            extent,
            capture: matched_node.capture,
            pattern: query_match.pattern(),
        });
    }
}

/// Whether the old captures of the matches rooted at `extent` can be kept. The re-run must
/// reproduce them exactly. Kept and re-queried captures are merged by start position, so no
/// two may start at the same position.
#[allow(clippy::too_many_arguments)]
fn anchor_holds(
    query: &Query,
    depths: &[Vec<(Capture, u32)>],
    spread: u32,
    node: &Node,
    source: RopeSlice,
    extent: &Range,
    old: &[LocalCapture],
    requeried: &[LocalCapture],
) -> bool {
    let Some((root, depth)) = anchor_root(node, extent, spread) else {
        return false;
    };
    let key = |range: &Range, capture: Capture, pattern: Pattern| {
        (range.start, range.end, capture.idx(), pattern)
    };
    let mut kept: Vec<_> = old
        .iter()
        .filter(|capture| capture.extent == *extent)
        .map(|capture| key(&capture.range, capture.capture, capture.pattern))
        .collect();
    if requeried
        .iter()
        .any(|capture| kept.iter().any(|kept| kept.0 == capture.range.start))
    {
        return false;
    }
    let mut cursor = InactiveQueryCursor::new(0..u32::MAX, TREE_SITTER_MATCH_LIMIT);
    cursor.set_max_start_depth(depth);
    let mut cursor = cursor.execute_query(query, &root, source);
    let mut rerun = Vec::new();
    while let Some(query_match) = cursor.next_match() {
        if match_extent(depths, &query_match) == *extent {
            rerun.extend(query_match.matched_nodes().map(|matched| {
                key(
                    &matched.node.byte_range(),
                    matched.capture,
                    query_match.pattern(),
                )
            }));
        }
    }
    kept.sort_unstable();
    rerun.sort_unstable();
    kept == rerun
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn cursor() {
        let mut locals = Locals::default();
        let scope1 = locals.push(ScopeData {
            definitions: Default::default(),
            range: 5..105,
            inherit: true,
            // NOTE: the subsequent call to `push` below will add scope2 to scope1's children.
            children: Default::default(),
            parent: Some(Scope::ROOT),
        });
        let scope2 = locals.push(ScopeData {
            definitions: Default::default(),
            range: 10..100,
            inherit: true,
            children: Default::default(),
            parent: Some(scope1),
        });

        let mut cursor = locals.scope_cursor(0);
        assert_eq!(cursor.current_scope(), Scope::ROOT);
        assert_eq!(cursor.advance(3), Scope::ROOT);
        assert_eq!(cursor.advance(5), scope1);
        assert_eq!(cursor.advance(8), scope1);
        assert_eq!(cursor.advance(10), scope2);
        assert_eq!(cursor.advance(50), scope2);
        assert_eq!(cursor.advance(100), scope1);
        assert_eq!(cursor.advance(105), Scope::ROOT);
        assert_eq!(cursor.advance(110), Scope::ROOT);

        let mut cursor = locals.scope_cursor(8);
        assert_eq!(cursor.current_scope(), scope1);
        assert_eq!(cursor.advance(10), scope2);
        assert_eq!(cursor.advance(100), scope1);
        assert_eq!(cursor.advance(110), Scope::ROOT);

        let mut cursor = locals.scope_cursor(10);
        assert_eq!(cursor.current_scope(), scope2);
        assert_eq!(cursor.advance(100), scope1);
        assert_eq!(cursor.advance(110), Scope::ROOT);
    }
}
