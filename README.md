# `tree-house`

This repository contains a number of crates used by the [Helix editor](https://github.com/helix-editor/helix) for integration with the [tree-sitter](https://github.com/tree-sitter/tree-sitter) C library.

Most notably the highlighter crate [`tree-house`](https://crates.io/crates/tree-house) provides Helix's syntax highlighting and all other tree-sitter features since the 25.07 release. The highlighter was rewritten from scratch for simplification and to fix a number of bugs. Read more in the [25.07 release highlights](https://helix-editor.com/news/release-25-07-highlights/#tree-house).

Documentation is a work-in-progress and these crates may see breaking changes as we expand our use of Tree-sitter in Helix.

* `bindings/` contains the `tree-house-bindings` crate which provides Rust bindings over the C library and optional integration with the [Ropey](https://github.com/cessen/ropey) rope crate.
* `highlighter/` contains the `tree-house` crate which exposes a robust highlighter and query iterator for working across [injections].
* `skidder/` contains the `skidder` crate which exposes utilities for building a package repository for tree-sitter grammars.
* `cli/` contains the `skidder-cli` crate which wraps `skidder` in a command line interface.

## Injection source prefilters

A query pattern can declare a necessary condition on the whole source with
`(#set! injection.source-pattern "/[/*]")`. If every injection pattern has a
prefilter and none matches, tree-house skips the injection query's tree traversal.
Patterns without this property keep the usual behavior. The regular expression
runs on the rope directly, including across chunk boundaries.

The query author must ensure that the expression matches every source in which
that pattern could produce an injection. Extra matches are safe: they only cause
normal query execution. For example, JSON comment injections can require `/[/*]`
because all JSON comments start with `//` or `/*`. This does not infer comments
from text: when the prefilter matches, the syntax query still decides which text
is a comment. Removing the last comment removes its injection normally.
