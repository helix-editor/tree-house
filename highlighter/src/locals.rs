use std::ops::Index;

use hashbrown::HashMap;
use kstring::KString;

use crate::highlighter::Highlight;
use crate::Range;

/// A layer represent a single a single syntax tree that reprsents (part of)
/// a file parsed with a tree-sitter grammar. See [`Syntax`]
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub struct Scope(u32);

impl Scope {
    const ROOT: Scope = Scope(0);
    fn idx(self) -> usize {
        self.0 as usize
    }
}

pub struct Locals {
    scopes: Vec<ScopeData>,
}

impl Locals {
    // pub(crate) fn new(range: Range<usize>) -> Locals {
    //     Locals {
    //         scopes: vec![ScopeData {
    //             defs: todo!(),
    //             range: todo!(),
    //             children: todo!(),
    //             inherit: todo!(),
    //             parent: todo!(),
    //         }],
    //     }
    // }
    pub fn lookup_reference(&self, mut scope: Scope, name: &str) -> Option<Highlight> {
        loop {
            let scope_data = &self[scope];
            if let Some(&highlight) = scope_data.defs.get(name) {
                return Some(highlight);
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
                .partition_point(|&child| pos < self[child].range.end);
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

    fn index(&self, scope: Scope) -> &ScopeData {
        &self.scopes[scope.idx()]
    }
}

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
            }
        }
        self.scope_stack.push((active_scope, child_idx));
        active_scope
    }
}

pub struct ScopeData {
    defs: HashMap<KString, Highlight>,
    range: Range,
    children: Box<[Scope]>,
    inherit: bool,
    parent: Option<Scope>,
}
