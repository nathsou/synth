// Weft for VS Code: a small Language Server Protocol client for `synth lsp`.
// It has no dependencies besides the VS Code API and Node's standard library.
'use strict';

const vscode = require('vscode');
const cp = require('child_process');

/** JSON-RPC over a child process's stdio. */
class Connection {
  constructor(command, args, log, trace) {
    this.log = log;
    this.trace = trace;
    this.nextId = 1;
    this.pending = new Map();
    this.handlers = new Map();
    this.buffer = Buffer.alloc(0);
    this.proc = cp.spawn(command, args, { stdio: ['pipe', 'pipe', 'pipe'] });
    this.proc.stdout.on('data', (d) => this.receive(d));
    this.proc.stderr.on('data', (d) => log.append(d.toString()));
    this.proc.on('error', (e) => {
      log.appendLine(`failed to start \`${command}\`: ${e.message}`);
      vscode.window.showErrorMessage(
        `Weft: could not start the language server (\`${command} ${args.join(' ')}\`). ` +
          'Set `weft.server.path` to the synth executable.'
      );
    });
    this.proc.on('exit', (code) => {
      log.appendLine(`language server exited (${code})`);
      for (const p of this.pending.values()) p.reject(new Error('server exited'));
      this.pending.clear();
      if (this.onExit) this.onExit(code);
    });
  }

  receive(data) {
    this.buffer = Buffer.concat([this.buffer, data]);
    for (;;) {
      const sep = this.buffer.indexOf('\r\n\r\n');
      if (sep < 0) return;
      const header = this.buffer.slice(0, sep).toString('ascii');
      const m = /content-length:\s*(\d+)/i.exec(header);
      const len = m ? parseInt(m[1], 10) : 0;
      if (this.buffer.length < sep + 4 + len) return;
      const body = this.buffer.slice(sep + 4, sep + 4 + len).toString('utf8');
      this.buffer = this.buffer.slice(sep + 4 + len);
      let msg;
      try {
        msg = JSON.parse(body);
      } catch {
        this.log.appendLine(`invalid message: ${body}`);
        continue;
      }
      if (this.trace) this.log.appendLine(`<- ${body}`);
      this.dispatch(msg);
    }
  }

  dispatch(msg) {
    if (msg.id !== undefined && msg.method === undefined) {
      const p = this.pending.get(msg.id);
      if (!p) return;
      this.pending.delete(msg.id);
      if (msg.error) p.reject(new Error(msg.error.message));
      else p.resolve(msg.result);
    } else if (msg.method) {
      const h = this.handlers.get(msg.method);
      if (h) h(msg.params);
      // Requests from the server are not used; answer them anyway.
      if (msg.id !== undefined) this.send({ jsonrpc: '2.0', id: msg.id, result: null });
    }
  }

  send(msg) {
    const body = JSON.stringify(msg);
    if (this.trace) this.log.appendLine(`-> ${body}`);
    if (!this.proc.stdin.writable) return;
    this.proc.stdin.write(`Content-Length: ${Buffer.byteLength(body, 'utf8')}\r\n\r\n${body}`);
  }

  request(method, params, token) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.send({ jsonrpc: '2.0', id, method, params });
      if (token) token.onCancellationRequested(() => this.notify('$/cancelRequest', { id }));
    });
  }

  notify(method, params) {
    this.send({ jsonrpc: '2.0', method, params });
  }

  on(method, handler) {
    this.handlers.set(method, handler);
  }

  stop() {
    return this.request('shutdown', null)
      .catch(() => {})
      .then(() => {
        this.notify('exit', null);
        setTimeout(() => this.proc.kill(), 500);
      });
  }
}

// Conversions between LSP and VS Code (both count UTF-16 code units).
const pos = (p) => new vscode.Position(p.line, p.character);
const range = (r) => new vscode.Range(pos(r.start), pos(r.end));
const location = (l) => new vscode.Location(vscode.Uri.parse(l.uri), range(l.range));
const docPos = (doc, p) => ({
  textDocument: { uri: doc.uri.toString() },
  position: { line: p.line, character: p.character },
});
const severity = [
  vscode.DiagnosticSeverity.Error,
  vscode.DiagnosticSeverity.Error,
  vscode.DiagnosticSeverity.Warning,
  vscode.DiagnosticSeverity.Information,
  vscode.DiagnosticSeverity.Hint,
];

function symbol(s) {
  const d = new vscode.DocumentSymbol(s.name, s.detail || '', s.kind - 1, range(s.range), range(s.selectionRange));
  d.children = (s.children || []).map(symbol);
  return d;
}

const TOKEN_TYPES = ['class', 'function', 'type', 'enumMember', 'property', 'parameter', 'variable', 'namespace', 'event', 'typeParameter'];
const TOKEN_MODIFIERS = ['declaration', 'readonly'];
const legend = new vscode.SemanticTokensLegend(TOKEN_TYPES, TOKEN_MODIFIERS);

let conn = null;
let output = null;
let diagnostics = null;
const versions = new Map();
const tokensChanged = new vscode.EventEmitter();

function isWeft(doc) {
  return doc.languageId === 'weft' && (doc.uri.scheme === 'file' || doc.uri.scheme === 'untitled');
}

function open(doc) {
  if (!conn || !isWeft(doc)) return;
  versions.set(doc.uri.toString(), doc.version);
  conn.notify('textDocument/didOpen', {
    textDocument: { uri: doc.uri.toString(), languageId: 'weft', version: doc.version, text: doc.getText() },
  });
}

function start() {
  const cfg = vscode.workspace.getConfiguration('weft');
  const command = cfg.get('server.path', 'synth');
  const args = cfg.get('server.args', ['lsp']);
  output.appendLine(`starting \`${command} ${args.join(' ')}\``);
  conn = new Connection(command, args, output, cfg.get('trace', false));
  const c = conn;
  c.on('textDocument/publishDiagnostics', (p) => {
    const uri = vscode.Uri.parse(p.uri);
    diagnostics.set(
      uri,
      p.diagnostics.map((d) => {
        const v = new vscode.Diagnostic(range(d.range), d.message, severity[d.severity || 1]);
        v.source = d.source;
        if (d.code) v.code = d.code;
        v.relatedInformation = (d.relatedInformation || []).map(
          (r) => new vscode.DiagnosticRelatedInformation(location(r.location), r.message)
        );
        return v;
      })
    );
    tokensChanged.fire();
  });
  c.request('initialize', {
    processId: process.pid,
    clientInfo: { name: 'vscode-weft' },
    rootUri: vscode.workspace.workspaceFolders ? vscode.workspace.workspaceFolders[0].uri.toString() : null,
    capabilities: { general: { positionEncodings: ['utf-16'] } },
  })
    .then(() => {
      c.notify('initialized', {});
      vscode.workspace.textDocuments.forEach(open);
    })
    .catch((e) => output.appendLine(`initialize failed: ${e.message}`));
}

function stop() {
  if (!conn) return Promise.resolve();
  const c = conn;
  conn = null;
  versions.clear();
  diagnostics.clear();
  return c.stop();
}

function ask(method, params, token) {
  if (!conn) return Promise.resolve(null);
  return conn.request(method, params, token).catch((e) => {
    output.appendLine(`${method}: ${e.message}`);
    return null;
  });
}

function activate(context) {
  output = vscode.window.createOutputChannel('Weft');
  diagnostics = vscode.languages.createDiagnosticCollection('weft');
  const sel = [{ language: 'weft' }];
  const sub = (d) => context.subscriptions.push(d);
  sub(output);
  sub(diagnostics);

  sub(vscode.workspace.onDidOpenTextDocument(open));
  sub(
    vscode.workspace.onDidChangeTextDocument((e) => {
      const doc = e.document;
      if (!conn || !isWeft(doc) || e.contentChanges.length === 0) return;
      versions.set(doc.uri.toString(), doc.version);
      conn.notify('textDocument/didChange', {
        textDocument: { uri: doc.uri.toString(), version: doc.version },
        contentChanges: [{ text: doc.getText() }],
      });
    })
  );
  sub(
    vscode.workspace.onDidSaveTextDocument((doc) => {
      if (conn && isWeft(doc)) conn.notify('textDocument/didSave', { textDocument: { uri: doc.uri.toString() } });
    })
  );
  sub(
    vscode.workspace.onDidCloseTextDocument((doc) => {
      if (!conn || !isWeft(doc)) return;
      versions.delete(doc.uri.toString());
      conn.notify('textDocument/didClose', { textDocument: { uri: doc.uri.toString() } });
    })
  );

  sub(
    vscode.languages.registerHoverProvider(sel, {
      provideHover: (doc, p, token) =>
        ask('textDocument/hover', docPos(doc, p), token).then((h) =>
          h ? new vscode.Hover(new vscode.MarkdownString(h.contents.value), h.range && range(h.range)) : null
        ),
    })
  );
  sub(
    vscode.languages.registerDefinitionProvider(sel, {
      provideDefinition: (doc, p, token) =>
        ask('textDocument/definition', docPos(doc, p), token).then((l) => (l ? location(l) : null)),
    })
  );
  sub(
    vscode.languages.registerReferenceProvider(sel, {
      provideReferences: (doc, p, ctx, token) =>
        ask('textDocument/references', { ...docPos(doc, p), context: { includeDeclaration: ctx.includeDeclaration } }, token).then(
          (ls) => (ls || []).map(location)
        ),
    })
  );
  sub(
    vscode.languages.registerDocumentHighlightProvider(sel, {
      provideDocumentHighlights: (doc, p, token) =>
        ask('textDocument/documentHighlight', docPos(doc, p), token).then((hs) =>
          (hs || []).map((h) => new vscode.DocumentHighlight(range(h.range)))
        ),
    })
  );
  sub(
    vscode.languages.registerDocumentSymbolProvider(sel, {
      provideDocumentSymbols: (doc, token) =>
        ask('textDocument/documentSymbol', { textDocument: { uri: doc.uri.toString() } }, token).then((ss) =>
          (ss || []).map(symbol)
        ),
    })
  );
  sub(
    vscode.languages.registerCompletionItemProvider(
      sel,
      {
        provideCompletionItems: (doc, p, token) =>
          ask('textDocument/completion', docPos(doc, p), token).then((r) =>
            ((r && r.items) || []).map((i) => {
              const c = new vscode.CompletionItem(i.label, i.kind - 1);
              c.detail = i.detail;
              return c;
            })
          ),
      },
      '.'
    )
  );
  sub(
    vscode.languages.registerRenameProvider(sel, {
      prepareRename: (doc, p, token) =>
        ask('textDocument/prepareRename', docPos(doc, p), token).then((r) => {
          if (!r) throw new Error('This name cannot be renamed.');
          return range(r);
        }),
      provideRenameEdits: (doc, p, newName, token) =>
        conn.request('textDocument/rename', { ...docPos(doc, p), newName }, token).then((r) => {
          const edit = new vscode.WorkspaceEdit();
          for (const [uri, edits] of Object.entries((r && r.changes) || {})) {
            for (const e of edits) edit.replace(vscode.Uri.parse(uri), range(e.range), e.newText);
          }
          return edit;
        }),
    })
  );
  sub(
    vscode.languages.registerDocumentSemanticTokensProvider(
      sel,
      {
        onDidChangeSemanticTokens: tokensChanged.event,
        provideDocumentSemanticTokens: (doc, token) =>
          ask('textDocument/semanticTokens/full', { textDocument: { uri: doc.uri.toString() } }, token).then((r) =>
            r ? new vscode.SemanticTokens(new Uint32Array(r.data)) : null
          ),
      },
      legend
    )
  );

  sub(
    vscode.commands.registerCommand('weft.restartServer', () => stop().then(start))
  );
  sub(vscode.commands.registerCommand('weft.showOutput', () => output.show()));
  sub(
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration('weft.server')) stop().then(start);
    })
  );
  start();
}

function deactivate() {
  return stop();
}

module.exports = { activate, deactivate };
