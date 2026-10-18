# Weft for Visual Studio Code

Syntax highlighting for [Weft](../../docs/WEFT.md) and a client for the
language server built into `synth` (`synth lsp`):

- errors and lint warnings as you type, with related locations;
- hover (types, value ranges, widths, ports of instances);
- go to definition, find references, highlights and rename, across the
  files of a design;
- outline, completion (`.` after an instance lists its ports) and semantic
  highlighting (registers, wires, ports, instances and domains).

The client is plain JavaScript with no dependencies.

## Install

Build `synth` (`cargo build --release`) and put it on your `PATH`, or set
`weft.server.path` to the executable. Then link this folder into your
extensions directory:

```sh
ln -s "$PWD/editors/vscode" ~/.vscode/extensions/synth.weft-0.1.0
```

and reload the window. `Weft: Restart Language Server` restarts it, and
`weft.trace` logs the protocol to the "Weft" output channel.

## Files analyzed together

Each file is checked with the `.weft` files of its directory that define
the names it uses, so a design split into several files works without a
project file. Files that would clash with it (another design's `top` in
the same folder) are left out. Verilog files in the directory define
`extern module`s.
