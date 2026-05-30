use std::mem::take;
use std::time::Duration;

use ropey::RopeSlice;
use tree_sitter::Parser;

use crate::config::LanguageLoader;
use crate::{Error, LayerData, Syntax};

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
        if source.len() >= 512 * 1024 * 1024 {
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
        parser.set_timeout(timeout);

        while let Some(layer) = queue.pop() {
            let layer_data = self.layer_mut(layer);
            if layer_data.ranges.is_empty() {
                // Skip re-parsing and querying layers without any ranges.
                continue;
            }
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
                    // Re-parse the tree.
                    layer_data.parse(&mut parser, source, loader)?;
                }
            } else {
                // always parse if this layer has never been parsed before
                layer_data.parse(&mut parser, source, loader)?;
            }
            self.run_injection_query(layer, edits, source, loader, |layer| queue.push(layer));
            self.run_local_query(layer, source, loader);
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
    fn parse(
        &mut self,
        parser: &mut Parser,
        source: RopeSlice,
        loader: &impl LanguageLoader,
    ) -> Result<(), Error> {
        let Some(config) = loader.get_config(self.language) else {
            return Ok(());
        };
        if let Err(err) = parser.set_grammar(config.grammar) {
            return Err(Error::IncompatibleGrammar(self.language, err));
        }
        parser
            .set_included_ranges(&self.ranges)
            .map_err(|_| Error::InvalidRanges)?;

        // HACK:
        // This is a workaround for a bug within the lexer (in the C library) or maybe within
        // tree-sitter-markdown which needs more debugging. When adding a new range to a combined
        // injection and passing the old tree, if the old tree doesn't already cover a wider range
        // than the newly added range, some assumptions are violated in the lexer and it tries to
        // access some invalid memory, resulting in a segfault. This workaround avoids that
        // situation by avoiding passing the old tree when the old tree's range doesn't cover the
        // total range of `self.ranges`.
        //
        // See <https://github.com/helix-editor/helix/pull/12972#issuecomment-2725410409>.
        let tree = self.parse_tree.as_ref().filter(|tree| {
            let included_ranges_range = self.ranges.first().map(|r| r.start_byte).unwrap_or(0)
                ..self.ranges.last().map(|r| r.end_byte).unwrap_or(u32::MAX);
            // Allow re-parsing the root layer even though the range is larger. The root always
            // covers `0..u32::MAX`:
            if included_ranges_range == (0..u32::MAX) {
                return true;
            }
            let tree_range = tree.root_node().byte_range();
            tree_range.start <= included_ranges_range.start
                && tree_range.end >= included_ranges_range.end
        });

        let tree = parser.parse(source, tree).ok_or(Error::Timeout)?;
        self.parse_tree = Some(tree);
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq, Default, Clone)]
pub(crate) struct LayerUpdateFlags {
    pub reused: bool,
    pub modified: bool,
    pub moved: bool,
    pub touched: bool,
}
