use std::mem::take;
use std::time::Duration;

use ropey::RopeSlice;
use tree_sitter::{Parser, Tree};

use crate::config::LanguageLoader;
use crate::injections_query::{merge_spans, EditMap};
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
        // The text each edit inserted, padded by a byte on both sides.
        // A deletion inserts nothing, and the node it shortened now ends at the deletion.
        let edit_spans = merge_spans(
            edited
                .iter()
                .map(|region| region.start.saturating_sub(1)..region.end + 1)
                .collect(),
        );

        while let Some(layer) = queue.pop() {
            let layer_data = self.layer_mut(layer);
            if layer_data.ranges.is_empty() {
                // Skip re-parsing and querying layers without any ranges.
                continue;
            }
            // The old tree stays valid without a re-parse when no edit touched the text under it
            // and its included ranges haven't changed. Edits that start after the layer's end
            // don't affect it.
            let end = layer_data.ranges.last().unwrap().end_byte;
            let reaching = edited.partition_point(|region| region.start <= end);
            let moved = reaching > 0;
            let reparse = match &mut layer_data.parse_tree {
                Some(tree) => {
                    if moved && edited[reaching - 1].end < layer_data.ranges[0].start_byte {
                        // These edits all end before the layer, so they only shift its tree.
                        tree.edit(&map.shift(reaching));
                    } else {
                        // Edits are in old coordinates: applying them in reverse keeps them valid.
                        for edit in edits[..reaching].iter().rev() {
                            tree.edit(edit);
                        }
                    }
                    touches(&layer_data.ranges, &edited)
                        || !tree_has_ranges(tree, &layer_data.ranges)
                }
                None => true,
            };
            // The regions to re-query. `None` re-queries everything.
            let spans = if reparse {
                // Text edits can change a match without changing the structure (e.g. renaming
                // an injected language), so the edits count as changed regions too.
                layer_data
                    .parse(&mut parser, source, timeout, loader)?
                    .map(|mut spans| {
                        spans.extend_from_slice(&edit_spans);
                        merge_spans(spans)
                    })
            } else {
                Some(Vec::new())
            };
            self.run_injection_query(
                layer,
                &map,
                source,
                loader,
                |layer| queue.push(layer),
                spans.clone(),
            );
            // A theme change can enable skipped definitions without changing the tree. The scopes
            // are then rebuilt from the cached captures.
            let layer_data = self.layer(layer);
            let locals_changed = loader
                .get_config(layer_data.language)
                .is_some_and(|config| layer_data.locals.configuration_changed(config));
            if reparse || locals_changed {
                self.run_local_query(layer, &map, source, loader, spans);
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
    /// With an old tree it returns the regions that structurally changed.
    /// `None` means assume everything changed.
    fn parse(
        &mut self,
        parser: &mut Parser,
        source: RopeSlice,
        timeout: Duration,
        loader: &impl LanguageLoader,
    ) -> Result<Option<Vec<Range>>, Error> {
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
        let changed = tree.map(|old_tree| {
            old_tree
                .changed_ranges(&new_tree)
                .iter()
                .map(|range| range.start_byte..range.end_byte)
                .collect()
        });
        self.parse_tree = Some(new_tree);
        Ok(changed)
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

/// Per-update flags. Reset when the update finishes.
#[derive(Debug, PartialEq, Eq, Default, Clone)]
pub(crate) struct LayerUpdateFlags {
    /// The layer is still referenced by its parent. Untouched layers are pruned.
    pub touched: bool,
    /// An injection of the parent's re-query already claimed the layer, so it can't be further reused.
    pub reused: bool,
}
