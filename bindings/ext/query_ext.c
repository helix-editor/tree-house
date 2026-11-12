// Extend tree-sitter's public API (upstream eventually, validated against v0.26.9)
// If the tree-sitter internals or semantics change, we detect it by failing to compile.
#include "lib.c"

// For each capture, writes to `depths[capture_id]` its depth below the pattern's root
// (UINT32_MAX if unused). `depths` must hold `ts_query_capture_count` entries. Returns
// whether the root is the parent of the pattern's top-level nodes.
bool ts_query_pattern_capture_depths(
  const TSQuery *self,
  uint32_t pattern_index,
  uint32_t *depths
) {
  const QueryPattern *pattern = array_get(&self->patterns, pattern_index);
  uint32_t start = pattern->steps.offset, end = start + pattern->steps.length;

  // Alternatives at the root are separated by dead ends, only siblings within one
  // alternative make a sequence.
  bool needs_parent = false, seen_root = false;
  for (uint32_t i = start; i < end; i++) {
    const QueryStep *step = array_get(&self->steps, i);
    if (step->depth != 0) continue;
    if (step->is_dead_end) {
      seen_root = false;
    } else if (step->is_pass_through) {
      needs_parent = true;
    } else {
      needs_parent |= seen_root;
      seen_root = true;
    }
  }

  for (uint32_t c = 0; c < ts_query_capture_count(self); c++) depths[c] = UINT32_MAX;
  for (uint32_t i = start; i < end; i++) {
    const QueryStep *step = array_get(&self->steps, i);
    if (step->depth == PATTERN_DONE_MARKER) continue;
    for (unsigned k = 0; k < MAX_STEP_CAPTURE_COUNT && step->capture_ids[k] != NONE; k++) {
      uint32_t *deepest = &depths[step->capture_ids[k]];
      uint32_t depth = step->depth + needs_parent;
      if (*deepest == UINT32_MAX || *deepest < depth) *deepest = depth;
    }
  }
  return needs_parent;
}

// The largest difference between the depths of one capture in the pattern's alternatives.
// A match can be rooted up to this many levels below where its capture depths place it.
uint32_t ts_query_pattern_depth_spread(const TSQuery *self, uint32_t pattern_index) {
  const QueryPattern *pattern = array_get(&self->patterns, pattern_index);
  uint32_t start = pattern->steps.offset, end = start + pattern->steps.length;
  uint32_t spread = 0;
  for (uint32_t c = 0; c < ts_query_capture_count(self); c++) {
    uint32_t min = UINT32_MAX, max = 0;
    for (uint32_t i = start; i < end; i++) {
      const QueryStep *step = array_get(&self->steps, i);
      if (step->depth == PATTERN_DONE_MARKER) continue;
      for (unsigned k = 0; k < MAX_STEP_CAPTURE_COUNT && step->capture_ids[k] != NONE; k++) {
        if (step->capture_ids[k] != c) continue;
        if (step->depth < min) min = step->depth;
        if (step->depth > max) max = step->depth;
      }
    }
    if (min != UINT32_MAX && max - min > spread) spread = max - min;
  }
  return spread;
}
