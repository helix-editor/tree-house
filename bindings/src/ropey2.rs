use std::ops;

use ropey2::{ChunkCursor, RopeSlice};

use crate::{Input, IntoInput};

pub struct RopeInput<'a> {
    src: RopeSlice<'a>,
    cursor: ChunkCursor<'a>,
}

impl<'a> RopeInput<'a> {
    pub fn new(src: RopeSlice<'a>) -> Self {
        RopeInput {
            src,
            cursor: src.chunk_cursor(),
        }
    }
}

impl<'a> IntoInput for RopeSlice<'a> {
    type Input = RopeInput<'a>;

    fn into_input(self) -> Self::Input {
        RopeInput::new(self)
    }
}

impl<'a> Input for RopeInput<'a> {
    type Cursor = ChunkCursor<'a>;
    fn cursor_at(&mut self, offset: u32) -> &mut ChunkCursor<'a> {
        let offset = offset as usize;
        debug_assert!(
            offset <= self.src.len(),
            "parser offset out of bounds: {offset} > {}",
            self.src.len()
        );
        // this cursor is optimized for contiguous reads which are by far the most common during parsing
        // very far jumps (like injections at the other end of the document) are handled
        // by starting a new cursor (new chunks iterator)
        if offset < self.cursor.byte_offset() || offset - self.cursor.byte_offset() > 4906 {
            self.cursor = self.src.chunk_cursor_at(offset);
        } else {
            while self.cursor.byte_offset() + self.cursor.chunk().len() <= offset {
                if !self.cursor.next() {
                    break;
                }
            }
        }
        &mut self.cursor
    }

    fn eq(&mut self, range1: ops::Range<u32>, range2: ops::Range<u32>) -> bool {
        let range1 = self.src.slice(range1.start as usize..range1.end as usize);
        let range2 = self.src.slice(range2.start as usize..range2.end as usize);
        range1 == range2
    }
}
