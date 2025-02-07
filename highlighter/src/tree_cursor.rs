use crate::tree_sitter::Node;
use crate::{Layer, Syntax};

pub struct TreeCursor<'tree> {
    syntax: &'tree Syntax,
    current: Layer,
    cursor: tree_sitter::TreeCursor<'tree>,
}

impl<'tree> TreeCursor<'tree> {
    pub(crate) fn new(syntax: &'tree Syntax) -> Self {
        let cursor = syntax.tree().walk();

        Self {
            syntax,
            current: syntax.root,
            cursor,
        }
    }

    pub fn node(&self) -> Node<'tree> {
        self.cursor.node()
    }

    pub fn goto_parent(&mut self) -> bool {
        if self.cursor.goto_parent() {
            return true;
        };

        // Ascend to the parent layer if one exists.
        let Some(parent) = self.syntax.layer(self.current).parent else {
            return false;
        };

        self.current = parent;
        self.cursor = self.syntax.layer(self.current).tree().walk();

        true
    }

    pub fn goto_parent_with<P>(&mut self, predicate: P) -> bool
    where
        P: Fn(&Node) -> bool,
    {
        while self.goto_parent() {
            if predicate(&self.node()) {
                return true;
            }
        }

        false
    }

    pub fn goto_first_child(&mut self) -> bool {
        let range = self.cursor.node().byte_range();
        let layer = self.syntax.layer(self.current);
        if let Some(injection) = layer
            .injection_at_byte_idx(range.start)
            .filter(|injection| injection.range.end >= range.end)
        {
            // Switch to the child layer.
            self.current = injection.layer;
            self.cursor = self.syntax.layer(self.current).tree().walk();
            return true;
        }

        self.cursor.goto_first_child()
    }

    pub fn goto_next_sibling(&mut self) -> bool {
        self.cursor.goto_next_sibling()
    }

    pub fn goto_previous_sibling(&mut self) -> bool {
        self.cursor.goto_previous_sibling()
    }

    pub fn reset_to_byte_range(&mut self, start: u32, end: u32) {
        let layer = self.syntax.layer_for_byte_range(start, end);
        self.current = layer;
        self.cursor = self.syntax.layer(self.current).tree().walk();

        loop {
            let node = self.cursor.node();
            if start < node.start_byte() || end > node.end_byte() {
                self.cursor.goto_parent();
                break;
            }
            if self.cursor.goto_first_child_for_byte(start).is_none() {
                break;
            }
        }
    }

    /// Returns an iterator over the children of the node the TreeCursor is on
    /// at the time this is called.
    pub fn children<'a>(&'a mut self) -> ChildIter<'a, 'tree> {
        let parent = self.node();

        ChildIter {
            cursor: self,
            parent,
        }
    }
}

pub struct ChildIter<'a, 'tree> {
    cursor: &'a mut TreeCursor<'tree>,
    parent: Node<'tree>,
}

impl<'tree> Iterator for ChildIter<'_, 'tree> {
    type Item = Node<'tree>;

    fn next(&mut self) -> Option<Self::Item> {
        // first iteration, just visit the first child
        if self.cursor.node() == self.parent {
            self.cursor.goto_first_child().then(|| self.cursor.node())
        } else {
            self.cursor.goto_next_sibling().then(|| self.cursor.node())
        }
    }
}
