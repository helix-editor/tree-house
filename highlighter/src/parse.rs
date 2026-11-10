use std::mem::take;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ropey::RopeSlice;
use tree_sitter::{ParseOptions, Parser, Tree};

use crate::config::LanguageLoader;
use crate::injections_query::{merge_spans, EditMap};
use crate::{Error, Layer, LayerData, Range, Syntax};

impl Syntax {
    /// Applies `edits` and re-parses. Same as [`Syntax::edit`] followed by [`Syntax::parse`].
    pub fn update(
        &mut self,
        source: RopeSlice,
        timeout: Duration,
        edits: &[tree_sitter::InputEdit],
        loader: &impl LanguageLoader,
    ) -> Result<(), Error> {
        self.edit(edits);
        self.parse(source, timeout, None, loader)
    }

    /// Applies `edits` to every layer without parsing, so the syntax stays usable for
    /// highlighting until the next [`Syntax::parse`]. Can be called several times before
    /// parsing.
    pub fn edit(&mut self, edits: &[tree_sitter::InputEdit]) {
        if edits.is_empty() {
            return;
        }
        let map = EditMap::new(edits);
        let layers: Vec<Layer> = self
            .layers
            .iter()
            .map(|(idx, _)| Layer(idx as u32))
            .collect();
        for &layer in &layers {
            let layer_data = self.layer_mut(layer);
            if let (Some(tree), Some(first), Some(last)) = (
                &mut layer_data.parse_tree,
                layer_data.ranges.first(),
                layer_data.ranges.last(),
            ) {
                // Edits that start after the layer's end don't affect it.
                let reaching = edits.partition_point(|edit| edit.start_byte <= last.end_byte);
                if reaching > 0 && edits[reaching - 1].old_end_byte < first.start_byte {
                    // These edits all end before the layer, so they only shift its tree.
                    tree.edit(&map.shift(reaching));
                } else {
                    // Edits are in old coordinates: applying them in reverse keeps them valid.
                    for edit in edits[..reaching].iter().rev() {
                        tree.edit(edit);
                    }
                }
            }
            layer_data.locals.map(&map);
            self.map_injections(layer, &map);
        }
        for &layer in &layers {
            self.set_child_ranges(layer);
        }
        let mut edited: Vec<Range> = self
            .edited
            .iter()
            .map(|region| map.down(region.start)..map.up(region.end))
            .collect();
        edited.extend(map.edited_regions());
        self.edited = merge_regions(edited);
    }

    /// Re-parses the layers touched by edits since the last successful parse, and re-runs
    /// their queries. Stops with [`Error::Cancelled`] once `cancel` is set. A parse
    /// that failed can be run again.
    pub fn parse(
        &mut self,
        source: RopeSlice,
        timeout: Duration,
        cancel: Option<&AtomicBool>,
        loader: &impl LanguageLoader,
    ) -> Result<(), Error> {
        // size limit of 512MiB, TS just cannot handle files this big (too
        // slow). Furthermore, TS uses 32 (signed) bit indices so this limit
        // must never be raised above 2GiB
        if source.len_bytes() >= 512 * 1024 * 1024 {
            return Err(Error::ExceededMaximumSize);
        }

        // A failed parse can leave the flags set.
        for (_, layer) in self.layers.iter_mut() {
            layer.flags = Default::default();
        }
        let mut queue = Vec::with_capacity(32);
        self.layer_mut(self.root).flags.touched = true;
        queue.push(self.root);

        let mut parser = Parser::new();
        let edited = self.edited.clone();
        // The text each edit inserted, padded by a byte on both sides.
        // A deletion inserts nothing, and the node it shortened now ends at the deletion.
        let edit_spans = merge_spans(
            edited
                .iter()
                .map(|region| region.start.saturating_sub(1)..region.end + 1)
                .collect(),
        );

        while let Some(layer) = queue.pop() {
            if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
                return Err(Error::Cancelled);
            }
            let layer_data = self.layer_mut(layer);
            if layer_data.ranges.is_empty() {
                // Skip re-parsing and querying layers without any ranges.
                continue;
            }
            // The old tree stays valid without a re-parse when no edit touched the text under it
            // and its included ranges haven't changed.
            let reparse = match &layer_data.parse_tree {
                Some(tree) => {
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
                    .parse(&mut parser, source, timeout, cancel, loader)?
                    .map(|mut spans| {
                        spans.extend_from_slice(&edit_spans);
                        merge_spans(spans)
                    })
            } else {
                Some(Vec::new())
            };
            self.run_injection_query(
                layer,
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
                self.run_local_query(layer, source, loader, spans);
            }
        }

        if self.layer(self.root).parse_tree.is_none() {
            return Err(Error::NoRootConfig);
        }

        self.prune_dead_layers();
        self.edited.clear();
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
        cancel: Option<&AtomicBool>,
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

        let deadline = Instant::now() + timeout;
        let mut cancelled = false;
        let mut progress = |_: &_| {
            cancelled = cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed));
            if cancelled || Instant::now() >= deadline {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let new_tree = parser.parse_with_options(source, tree, ParseOptions::new(&mut progress));
        let Some(new_tree) = new_tree else {
            return Err(if cancelled {
                Error::Cancelled
            } else {
                Error::Timeout
            });
        };
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

/// Sorts `regions` and merges those that overlap or touch. Unlike [`merge_spans`] it keeps
/// empty regions, which mark deletions.
fn merge_regions(mut regions: Vec<Range>) -> Vec<Range> {
    regions.sort_unstable_by_key(|region| region.start);
    regions.dedup_by(|next, prev| {
        let merge = next.start <= prev.end;
        if merge {
            prev.end = prev.end.max(next.end);
        }
        merge
    });
    regions
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

/// Per-parse flags. Reset when the parse finishes.
#[derive(Debug, PartialEq, Eq, Default, Clone)]
pub(crate) struct LayerUpdateFlags {
    /// The layer is still referenced by its parent. Untouched layers are pruned.
    pub touched: bool,
    /// An injection of the parent's re-query already claimed the layer, so it can't be further reused.
    pub reused: bool,
}
