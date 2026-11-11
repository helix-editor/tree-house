use std::fmt;
use std::ops::Deref;
use std::os::raw::c_void;
use std::ptr::{self, NonNull};
use std::slice;

use crate::node::{Node, NodeRaw};
use crate::{Point, Range, TreeCursor};

// opaque pointers
pub(super) enum SyntaxTreeData {}

pub struct Tree {
    ptr: NonNull<SyntaxTreeData>,
}

impl Tree {
    pub(super) unsafe fn from_raw(raw: NonNull<SyntaxTreeData>) -> Tree {
        Tree { ptr: raw }
    }

    pub(super) fn as_raw(&self) -> NonNull<SyntaxTreeData> {
        self.ptr
    }

    pub fn root_node(&self) -> Node<'_> {
        unsafe { Node::from_raw(ts_tree_root_node(self.ptr)).unwrap() }
    }

    pub fn edit(&mut self, edit: &InputEdit) {
        unsafe { ts_tree_edit(self.ptr, edit) }
    }

    pub fn walk(&self) -> TreeCursor<'_> {
        self.root_node().walk()
    }

    /// Compare `self` to `other` and return the ranges whose syntactic structure changed,
    /// in `other`'s coordinates.
    ///
    /// `self` must be the old tree with all [edits](Tree::edit) applied, i.e. the tree that
    /// was passed to the parser to produce `other`. Ranges where only the text changed are
    /// not reported.
    pub fn changed_ranges(&self, other: &Tree) -> RangeArray {
        let mut len: u32 = 0;
        let ptr = unsafe { ts_tree_get_changed_ranges(self.ptr, other.ptr, &mut len) };
        RangeArray { ptr, len }
    }
}

/// An array of [`Range`]s allocated by tree-sitter on the caller's behalf.
pub struct RangeArray {
    ptr: *mut Range,
    len: u32,
}

impl Deref for RangeArray {
    type Target = [Range];

    fn deref(&self) -> &[Range] {
        if self.len == 0 {
            &[]
        } else {
            unsafe { slice::from_raw_parts(self.ptr, self.len as usize) }
        }
    }
}

impl Drop for RangeArray {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // Free with the allocator that produced the array
            unsafe { (ptr::addr_of!(ts_current_free).read())(self.ptr.cast()) };
        }
    }
}

impl fmt::Debug for RangeArray {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl fmt::Debug for Tree {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{{Tree {:?}}}", self.root_node())
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        unsafe { ts_tree_delete(self.ptr) }
    }
}

impl Clone for Tree {
    fn clone(&self) -> Self {
        unsafe {
            Tree {
                ptr: ts_tree_copy(self.ptr),
            }
        }
    }
}

unsafe impl Send for Tree {}
unsafe impl Sync for Tree {}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct InputEdit {
    pub start_byte: u32,
    pub old_end_byte: u32,
    pub new_end_byte: u32,
    pub start_point: Point,
    pub old_end_point: Point,
    pub new_end_point: Point,
}

impl InputEdit {
    /// returns the offset between the old end of the edit and the new end of
    /// the edit. This offset needs to be added to every position that occurs
    /// after `self.old_end_byte` to may it to its old position
    ///
    /// This function assumes that the the source-file is smaller than 2GiB
    pub fn offset(&self) -> i32 {
        self.new_end_byte as i32 - self.old_end_byte as i32
    }
}

extern "C" {
    /// Create a shallow copy of the syntax tree. This is very fast. You need to
    /// copy a syntax tree in order to use it on more than one thread at a time,
    /// as syntax trees are not thread safe.
    fn ts_tree_copy(self_: NonNull<SyntaxTreeData>) -> NonNull<SyntaxTreeData>;
    /// Delete the syntax tree, freeing all of the memory that it used.
    fn ts_tree_delete(self_: NonNull<SyntaxTreeData>);
    /// Get the root node of the syntax tree.
    fn ts_tree_root_node<'tree>(self_: NonNull<SyntaxTreeData>) -> NodeRaw;
    /// Edit the syntax tree to keep it in sync with source code that has been
    /// edited.
    ///
    /// You must describe the edit both in terms of byte offsets and in terms of
    /// row/column coordinates.
    fn ts_tree_edit(self_: NonNull<SyntaxTreeData>, edit: &InputEdit);
    /// Compare an old edited syntax tree to a new syntax tree representing the same
    /// document, returning an array of ranges whose syntactic structure has changed.
    /// The caller must free the returned array.
    fn ts_tree_get_changed_ranges(
        old_tree: NonNull<SyntaxTreeData>,
        new_tree: NonNull<SyntaxTreeData>,
        length: *mut u32,
    ) -> *mut Range;
    /// The deallocator tree-sitter currently uses (see `ts_set_allocator`).
    static mut ts_current_free: unsafe extern "C" fn(ptr: *mut c_void);
}
