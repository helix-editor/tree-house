use std::{
    borrow::Cow,
    ops::{Index, IndexMut},
};

use hashbrown::HashMap;
use kstring::KString;
use ropey::RopeSlice;
use tree_sitter::{Capture, InactiveQueryCursor};

use crate::{LanguageConfig, LanguageLoader, Layer, Range, Syntax, TREE_SITTER_MATCH_LIMIT};

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub struct Scope(u32);

impl Scope {
    const ROOT: Scope = Scope(0);
    fn idx(self) -> usize {
        self.0 as usize
    }
}

#[derive(Debug)]
pub struct Locals {
    scopes: Vec<ScopeData>,
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

        Self { scopes }
    }
}

impl Locals {
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

#[derive(Debug)]
pub struct Definition {
    pub capture: Capture,
    pub range: Range,
}

#[derive(Debug)]
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
    pub(crate) fn run_local_query(
        &mut self,
        layer: Layer,
        source: RopeSlice<'_>,
        loader: &impl LanguageLoader,
    ) {
        let layer_data = &mut self.layer_mut(layer);
        let Some(LanguageConfig {
            ref injection_query,
            ..
        }) = loader.get_config(layer_data.language)
        else {
            return;
        };
        let definition_captures = injection_query.local_definition_captures.load();
        if definition_captures.is_empty() {
            return;
        }

        let root = layer_data.parse_tree.as_ref().unwrap().root_node();
        let mut cursor = InactiveQueryCursor::new(0..u32::MAX, TREE_SITTER_MATCH_LIMIT)
            .execute_query(&injection_query.local_query, &root, source);
        let mut locals = Locals::default();
        let mut scope = Scope::ROOT;

        while let Some((query_match, node_idx)) = cursor.next_matched_node() {
            let matched_node = query_match.matched_node(node_idx);
            let range = matched_node.node.byte_range();
            let capture = matched_node.capture;

            while range.start >= locals[scope].range.end {
                scope = locals[scope].parent.expect("root node covers entire range");
            }

            if Some(capture) == injection_query.local_scope_capture {
                scope = locals.push(ScopeData {
                    definitions: HashMap::new(),
                    range: matched_node.node.byte_range(),
                    inherit: !injection_query
                        .not_scope_inherits
                        .contains(&query_match.pattern()),
                    children: Vec::new(),
                    parent: Some(scope),
                });
            } else if definition_captures.contains_key(&capture) {
                let text = match source
                    .slice(range.start as usize..range.end as usize)
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

        layer_data.locals = locals;
    }
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
