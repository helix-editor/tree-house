# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- ## [Unreleased] -->

## [v0.3.1] - 2026-05-30

> [!NOTE]
> This version does not have any breaking API changes to the highlighter, but the dependency on the bindings (`tree-house-bindings`) has been updated to require v0.3, which includes breaking changes to parsing functions.

### Added

* Derived `Hash` for `Highlight` ([bd161d96fac8](https://github.com/helix-editor/tree-house/commit/bd161d96fac8))
* Derived `Clone` for `Syntax` ([0fe37c6cc48b](https://github.com/helix-editor/tree-house/commit/0fe37c6cc48b))

### Fixed

* Fixed a panic in `Highlighter::advance` when multiple captures match the same node and more than one has no highlight in the current theme ([5734850e](https://github.com/helix-editor/tree-house/commit/5734850e), [helix#14751](https://github.com/helix-editor/helix/issues/14751), [#37](https://github.com/helix-editor/tree-house/issues/37))
* Fixed a silent bug where a child node's capture could replace an ancestor node's highlight in `active_highlights` when they share an end byte ([5734850e](https://github.com/helix-editor/tree-house/commit/5734850e))

## [v0.3.0] - 2025-06-16

### Fixed

* Fixed a bug where a parent node's first child being captured before the parent node caused the list of active highlights to become out-of-order.
* Fixed an issue where a combined injection would not have its active highlights retained until the next injection range if that injection range did not have any captures or injections itself.

### Updated

* The minimum required Rust version has been increased to 1.82.

## [v0.2.0] - 2025-06-06

### Added

* Added `Syntax::layers_for_byte_range`
* Added `TreeCursor::reset`
* Added an iterator for recursively walking over the nodes in a `TreeCursor`: `TreeRecursiveWalker`

### Changed

* `InactiveQueryCursor::new` now takes the byte range and match limit as parameters

### Fixed

* Included `LICENSE` in the crate package
* Fixed an issue where a combined injection layer could be queried multiple times by `QueryIter`
* Fixed an issue where a combined injection layer would not be re-parsed when an injection for the layer was removed by an edit

## [v0.1.0] - 2025-05-13

### Added

* Initial publish

