# 0007: A language server and a web playground

Status: Accepted

## Context

Weft (0006) has its value in its diagnostics, which are most useful in the
editor, while the code is written. The flow from source to bitstream is also
hard to see into: the passes, the gates, the placement, the routing and the
bits are only visible as log lines and output files. Both need presentation
code outside the toolchain's core, under the dependency policy of 0002.

## Decision

- `synth lsp` is a language server for Weft, in a new crate, `synth-lsp`,
  using only `std`. It speaks JSON-RPC over stdio through a small JSON type in
  `synth-core`.
- Its knowledge comes from the elaborator: `synth_weft::analyze` elaborates
  every module (with or without a top) and records what each name refers to,
  with hover text. There is no second, approximate analysis for the editor.
- Editor integrations are configuration plus, for VS Code, a client written
  against the VS Code API alone (`editors/vscode`), without
  `vscode-languageclient`.
- `synth-playground` turns the flow into data for a web app: the design after
  each pass as graphs, the placed and routed die, timing, and the configuration
  bits decoded back from the bitstream. iCE40 synthesis gains observation points
  and the flow a detailed result for this.
- The app is plain JavaScript modules, HTML and CSS: no framework, no bundler,
  no package manager, no CDN. It draws with WebGL2 and Canvas2D.
- It runs two ways from the same code: the flow compiled to WebAssembly, in
  workers, for a static deployment, or natively behind `synth playground`, an
  HTTP server built on `std::net` that embeds the app.

## Consequences

- The playground and the editor show the toolchain's own diagnostics and
  results; they cannot drift from it.
- Hierarchy is rebuilt from names, so the Weft elaborator now names anonymous
  cells and wires after their instance path (`uart.$binary12`).
- `synth_core::time::Instant` reads the clock through an import on WebAssembly
  (`env.synth_now_ms`), which any page loading the module must provide.
- The JavaScript has no type checker. It is kept to small modules; the pure
  ones (layout, highlighting) are unit-tested with Node's built-in test runner,
  and CI checks the syntax of all of them and builds the WebAssembly target.
  The views themselves are checked by hand in a browser.
- The playground supports iCE40 only, like the backend. Another family needs
  its own die description and bit decoder in `chip.rs`.
