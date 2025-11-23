use crate::TREE_SITTER_MATCH_LIMIT;
use ropey::RopeSlice;
use std::{iter, ops::Range};
use tree_sitter::{InactiveQueryCursor, Node, Query, RopeInput};

#[derive(Debug)]
pub struct TagQuery {
    pub query: Query,
}

pub struct Tag<'a> {
    /// The byte range of the captured node
    pub range: Range<u32>,
    /// The tag that was captured
    pub category: TagCategory<'a>,
    /// A slice, that corresponds to the captured `@name` query, if one
    /// was found.
    pub name: Option<RopeSlice<'a>>,
}

pub enum TagCategory<'a> {
    /// A definition, i.e. a query, that starts with `definition.`, with the
    /// prefix removed.
    Definition(&'a str),
}

impl<'a> TagCategory<'a> {
    fn from_name(name: &'a str) -> Option<TagCategory<'a>> {
        name.strip_prefix("definition.")
            .map(TagCategory::Definition)
    }
}

impl TagQuery {
    pub fn tags<'a>(
        &'a self,
        node: Node<'a>,
        text: RopeSlice<'a>,
    ) -> impl Iterator<Item = Tag<'a>> {
        let mut cursor = InactiveQueryCursor::new(0..u32::MAX, TREE_SITTER_MATCH_LIMIT)
            .execute_query(&self.query, &node, RopeInput::new(text));

        iter::from_fn(move || loop {
            let mat = cursor.next_match()?;

            let mut kind = None;
            let mut tag_name = None;

            for matched in mat.matched_nodes() {
                let name = matched.capture.name(&self.query);
                if name == "name" {
                    let start = text.byte_to_char(matched.node.start_byte() as usize);
                    let end = text.byte_to_char(matched.node.end_byte() as usize);

                    tag_name = Some(text.slice(start..end));
                } else if let Some(nkind) = TagCategory::from_name(name) {
                    kind = Some((nkind, matched.node.byte_range()));
                }
            }

            let Some((category, range)) = kind else {
                continue;
            };

            return Some(Tag {
                range,
                category,
                name: tag_name,
            });
        })
    }
}
