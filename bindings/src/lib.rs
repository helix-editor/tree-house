mod grammar;
mod node;
mod parser;
pub mod query;
mod query_cursor;
mod tree;
mod tree_cursor;

#[cfg(feature = "ropey")]
mod ropey;
#[cfg(feature = "ropey")]
pub use ropey::RopeInput;

#[cfg(feature = "ropey2")]
mod ropey2;
#[cfg(feature = "ropey2")]
pub use ropey2::RopeInput as Ropey2Input;

use std::ops;

pub use grammar::{Grammar, IncompatibleGrammarError};
pub use node::Node;
pub use parser::{Parser, ParserInputRaw};
pub use query::{Capture, Pattern, Query, QueryStr};
pub use query_cursor::{InactiveQueryCursor, MatchedNode, MatchedNodeIdx, QueryCursor, QueryMatch};
pub use tree::{InputEdit, Tree};
pub use tree_cursor::TreeCursor;

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Point {
    pub row: u32,
    pub col: u32,
}

impl Point {
    pub const ZERO: Self = Self { row: 0, col: 0 };
    pub const MAX: Self = Self {
        row: u32::MAX,
        col: u32::MAX,
    };
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Range {
    pub start_point: Point,
    pub end_point: Point,
    pub start_byte: u32,
    pub end_byte: u32,
}

pub trait Input {
    type Cursor: regex_cursor::Cursor;
    fn cursor_at(&mut self, offset: u32) -> &mut Self::Cursor;
    fn eq(&mut self, range1: ops::Range<u32>, range2: ops::Range<u32>) -> bool;
}

pub trait IntoInput {
    type Input: Input;
    fn into_input(self) -> Self::Input;
}

impl<T: Input> IntoInput for T {
    type Input = T;

    fn into_input(self) -> T {
        self
    }
}
