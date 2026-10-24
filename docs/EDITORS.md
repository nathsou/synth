# Editor support for Weft

`synth lsp` is a language server for Weft, speaking the Language Server
Protocol over standard input and output. It gives any LSP-capable editor:

- errors and lint warnings as you type, with the related locations of each
  message and its `help:` suggestions;
- hover: the type, value range, width and clock domain of signals, the
  signature and `///` doc comment of modules and functions (the standard
  library's included), what each builtin does, the ports of instances;
- go to definition, find references, highlights of the name under the cursor,
  and rename, across the files of a design;
- the outline of a file (modules with their ports and declarations,
  functions, structs, enums, constants);
- completion of names, keywords and builtins, and after `.` the ports of an
  instance, the fields of a struct or the variants of an enum;
- semantic highlighting: registers, wires, ports, instances, domains,
  constants and types each get their own token type;
- formatting of a whole file, the same as `synth fmt` (see the layout rules
  in [WEFT.md §4](WEFT.md#4-source-files-and-items)).

## Which files are checked together

A Weft design is often split over files without a project file, and a
directory may hold several designs (the examples directory does). Each file is
checked with the `.weft` files of its directory that define the names it uses,
directly or through those files. A file whose definitions clash with ones
already in the group, typically another design's `top`, is left out. Verilog
files in the directory define `extern module`s when a Weft file declares one.
Modules that no top instantiates are checked on their own, so a library file
gets diagnostics too.

## VS Code

[`editors/vscode`](../editors/vscode) is an extension with a TextMate grammar
and a small client for the server, without dependencies. See its README; in
short:

```sh
cargo build --release   # and put target/release on your PATH,
                        # or set weft.server.path
ln -s "$PWD/editors/vscode" ~/.vscode/extensions/synth.weft-0.1.0
```

## Neovim

```lua
vim.filetype.add({ extension = { weft = 'weft' } })
vim.api.nvim_create_autocmd('FileType', {
  pattern = 'weft',
  callback = function(ev)
    vim.lsp.start({
      name = 'synth',
      cmd = { 'synth', 'lsp' },
      root_dir = vim.fs.dirname(ev.file),
    })
  end,
})
```

## Helix

In `languages.toml`:

```toml
[language-server.synth]
command = "synth"
args = ["lsp"]

[[language]]
name = "weft"
scope = "source.weft"
file-types = ["weft"]
comment-token = "//"
indent = { tab-width = 4, unit = "    " }
language-servers = ["synth"]
```

## Emacs (Eglot)

```elisp
(define-derived-mode weft-mode prog-mode "Weft")
(add-to-list 'auto-mode-alist '("\\.weft\\'" . weft-mode))
(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs '(weft-mode "synth" "lsp")))
```

## Implementation

`crates/synth-lsp` is the server (JSON-RPC framing, position conversion in
UTF-16 or UTF-8 as negotiated, the requests above). It calls
`synth_weft::analyze`, which elaborates every module even when no top can be
inferred and records, for each name the elaborator resolves, its declaration,
a hover description and its members. Requests about a file that changed since
its last analysis analyze it first; other edits are analyzed once the queued
messages are handled.
