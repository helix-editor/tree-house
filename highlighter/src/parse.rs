use std::mem::take;
use std::time::Duration;

use ropey::RopeSlice;
use tree_sitter::{Parser, Tree};

use crate::config::LanguageLoader;
use crate::injections_query::EditMap;
use crate::{Error, LayerData, Range, Syntax};

impl Syntax {
    pub fn update(
        &mut self,
        source: RopeSlice,
        timeout: Duration,
        edits: &[tree_sitter::InputEdit],
        loader: &impl LanguageLoader,
    ) -> Result<(), Error> {
        // size limit of 512MiB, TS just cannot handle files this big (too
        // slow). Furthermore, TS uses 32 (signed) bit indices so this limit
        // must never be raised above 2GiB
        if source.len_bytes() >= 512 * 1024 * 1024 {
            return Err(Error::ExceededMaximumSize);
        }

        let mut queue = Vec::with_capacity(32);
        self.layer_mut(self.root).flags.touched = true;
        queue.push(self.root);

        let mut parser = Parser::new();
        let map = EditMap::new(edits);
        let edited = map.edited_regions();
        let edit_span = edited.first().map_or(0..0, |first| {
            // The text each edit inserted, padded by a byte on both sides.
            // A deletion inserts nothing, and the node it shortened now ends at the deletion.
            first.start.saturating_sub(1)..edited.last().unwrap().end + 1
        });

        while let Some(layer) = queue.pop() {
            let is_root = layer == self.root;
            let layer_data = self.layer_mut(layer);
            if layer_data.ranges.is_empty() {
                // Skip re-parsing and querying layers without any ranges.
                continue;
            }
            // The old tree stays valid without a re-parse when no edit touched the text under it
            // and its included ranges haven't changed. Text before the first edit has the same
            // offsets in old and new coordinates, so an edit starting past the layer's end
            // doesn't move it.
            let moved = edited
                .first()
                .is_some_and(|first| first.start <= layer_data.ranges.last().unwrap().end_byte);
            let reparse = match &mut layer_data.parse_tree {
                Some(tree) => {
                    if moved {
                        // Edits are in old coordinates: applying them in reverse keeps them valid.
                        for edit in edits.iter().rev() {
                            tree.edit(edit);
                        }
                    }
                    touches(&layer_data.ranges, &edited)
                        || !tree_has_ranges(tree, &layer_data.ranges)
                }
                None => true,
            };
            // With a span, only the injections intersecting it are re-queried.
            let span = if reparse {
                // Text edits can change a match without changing the structure (e.g. renaming
                // an injected language), so the edits count as changed regions too.
                layer_data
                    .parse(&mut parser, source, timeout, loader, is_root)?
                    .map(|span| union_ranges(span, edit_span.clone()))
            } else {
                Some(0..0)
            };
            self.run_injection_query(layer, &map, source, loader, |layer| queue.push(layer), span);
            if reparse {
                self.run_local_query(layer, source, loader);
            } else if moved {
                self.layer_mut(layer).locals.map(&map);
            }
        }

        if self.layer(self.root).parse_tree.is_none() {
            return Err(Error::NoRootConfig);
        }

        self.prune_dead_layers();
        Ok(())
    }

    /// Reset all `LayerUpdateFlags` and remove all untouched layers
    fn prune_dead_layers(&mut self) {
        self.layers
            .retain(|_, layer| take(&mut layer.flags).touched);
    }
}

impl LayerData {
    /// Parses this layer's tree.
    ///
    /// With `narrow` and an old tree it returns the span of regions that structurally changed.
    /// `None` means assume everything changed.
    fn parse(
        &mut self,
        parser: &mut Parser,
        source: RopeSlice,
        timeout: Duration,
        loader: &impl LanguageLoader,
        narrow: bool,
    ) -> Result<Option<Range>, Error> {
        let Some(config) = loader.get_config(self.language) else {
            return Ok(None);
        };
        if let Err(err) = parser.set_grammar(config.grammar) {
            return Err(Error::IncompatibleGrammar(self.language, err));
        }
        parser
            .set_included_ranges(&self.ranges)
            .map_err(|_| Error::InvalidRanges)?;

        // Only seed the parse with a tree parsed under the same ranges. Otherwise tree-sitter
        // reuses subtrees and scanner state in the wrong context, which misparses and has
        // crashed tree-sitter-markdown.
        let tree = self
            .parse_tree
            .as_ref()
            .filter(|tree| tree_has_ranges(tree, &self.ranges));

        let new_tree = parser
            .parse_with_timeout(source, tree, timeout)
            .ok_or(Error::Timeout)?;
        let changed_span = tree.filter(|_| narrow).map(|old_tree| {
            old_tree
                .changed_ranges(&new_tree)
                .iter()
                .fold(0..0, |span, range| {
                    union_ranges(span, range.start_byte..range.end_byte)
                })
        });
        self.parse_tree = Some(new_tree);
        Ok(changed_span)
    }
}

fn tree_has_ranges(tree: &Tree, ranges: &[tree_sitter::Range]) -> bool {
    let included = tree.included_ranges();
    included.len() == ranges.len()
        && included
            .iter()
            .zip(ranges)
            .all(|(old, new)| old.start_byte == new.start_byte && old.end_byte == new.end_byte)
}

/// Whether any of the sorted `ranges` intersects or touches any of the sorted `regions`.
fn touches(ranges: &[tree_sitter::Range], regions: &[Range]) -> bool {
    regions.iter().any(|region| {
        let i = ranges.partition_point(|range| range.end_byte < region.start);
        ranges
            .get(i)
            .is_some_and(|range| range.start_byte <= region.end)
    })
}

fn union_ranges(a: Range, b: Range) -> Range {
    if a.is_empty() {
        b
    } else if b.is_empty() {
        a
    } else {
        a.start.min(b.start)..a.end.max(b.end)
    }
}

/// Per-update flags. Reset when the update finishes.
#[derive(Debug, PartialEq, Eq, Default, Clone)]
pub(crate) struct LayerUpdateFlags {
    /// The layer is still referenced by its parent. Untouched layers are pruned.
    pub touched: bool,
    /// An injection of the parent's re-query already claimed the layer, so it can't be further reused.
    pub reused: bool,
}
