use std::mem::take;
use std::time::Duration;

use ropey::RopeSlice;
use tree_sitter::Parser;

use crate::config::LanguageLoader;
use crate::{Error, Layer, LayerData, Range, Syntax};

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
        let root_flags = &mut self.layer_mut(self.root).flags;
        // The root layer is always considered.
        root_flags.touched = true;
        // If there was an edit then the root layer must've been modified.
        root_flags.modified = true;
        queue.push(self.root);

        let mut parser = Parser::new();
        let edit_span = edit_span(edits);

        while let Some(layer) = queue.pop() {
            let is_root = layer == self.root;
            let layer_data = self.layer_mut(layer);
            if layer_data.ranges.is_empty() {
                // Skip re-parsing and querying layers without any ranges.
                continue;
            }
            let mut fresh = false;
            let mut changed_span = None;
            if let Some(tree) = &mut layer_data.parse_tree {
                if layer_data.flags.moved || layer_data.flags.modified {
                    for edit in edits.iter().rev() {
                        // Apply the edits in reverse.
                        // If we applied them in order then edit 1 would disrupt the positioning
                        // of edit 2.
                        tree.edit(edit);
                    }
                }
                if layer_data.flags.modified {
                    // Re-parse the tree. Comparing the old and new tree is only sound when the
                    // included ranges cannot have changed, which holds for the root's whole-document range.
                    changed_span =
                        layer_data.parse(&mut parser, source, timeout, loader, is_root)?;
                }
            } else {
                // always parse if this layer has never been parsed before
                layer_data.parse(&mut parser, source, timeout, loader, false)?;
                fresh = true;
            }
            if fresh || self.layer(layer).flags.modified {
                // Text edits can change a match without changing the structure (e.g. renaming
                // an injected language), so the edits count as changed regions too.
                let span = changed_span.map(|span| union_ranges(span, edit_span.clone()));
                self.run_injection_query(
                    layer,
                    edits,
                    source,
                    loader,
                    |layer| queue.push(layer),
                    span,
                );
                self.run_local_query(layer, source, loader);
            } else {
                self.map_unmodified_layer(layer, edits, |layer| queue.push(layer));
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

    /// Carries a layer without edits inside its ranges over without re-running its injection
    /// and locals queries: contents only shift by the offsets of the edits before them.
    fn map_unmodified_layer(
        &mut self,
        layer: Layer,
        edits: &[tree_sitter::InputEdit],
        mut parse_layer: impl FnMut(Layer),
    ) {
        // Children of an unmodified layer can only have moved.
        self.map_injections(layer, None, edits);
        let layer_data = self.layer_mut(layer);
        if layer_data.flags.moved {
            layer_data.locals.map(edits);
        }
        let injections = take(&mut self.layer_mut(layer).injections);
        for injection in &injections {
            let child = self.layer_mut(injection.layer);
            if !child.flags.touched {
                child.flags.touched = true;
                child.ranges.clear();
                parse_layer(injection.layer);
            }
            child.ranges.push(tree_sitter::Range::new(
                tree_sitter::Point::ZERO,
                tree_sitter::Point::ZERO,
                injection.range.start,
                injection.range.end,
            ));
        }
        self.layer_mut(layer).injections = injections;
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

        // An old tree may only seed the parse under exactly the included ranges it was parsed
        // with (as moved by the edits). Otherwise tree-sitter reuses subtrees and resumes
        // external scanner state in contexts they were not produced in, which misparses and
        // has crashed tree-sitter-markdown's block scanner.
        let tree = self.parse_tree.as_ref().filter(|tree| {
            let included = tree.included_ranges();
            included.len() == self.ranges.len()
                && included.iter().zip(&self.ranges).all(|(old, new)| {
                    old.start_byte == new.start_byte && old.end_byte == new.end_byte
                })
        });

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

fn union_ranges(a: Range, b: Range) -> Range {
    if a.is_empty() {
        b
    } else if b.is_empty() {
        a
    } else {
        a.start.min(b.start)..a.end.max(b.end)
    }
}

/// The span covered by `edits`, in new-text coordinates, or an empty range without edits.
///
/// Each edit's region is widened by a byte on both sides: a deletion leaves the nodes it
/// shortened as touching the edit's position (they must still count as edited).
fn edit_span(edits: &[tree_sitter::InputEdit]) -> Range {
    let mut span = 0..0;
    let mut offset = 0i32;
    for edit in edits {
        let new_start = (edit.start_byte as i32 + offset) as u32;
        let new_end = (edit.new_end_byte as i32 + offset) as u32;
        offset += edit.offset();
        span = union_ranges(span, new_start.saturating_sub(1)..new_end + 1);
    }
    span
}

#[derive(Debug, PartialEq, Eq, Default, Clone)]
pub(crate) struct LayerUpdateFlags {
    pub reused: bool,
    pub modified: bool,
    pub moved: bool,
    pub touched: bool,
}
