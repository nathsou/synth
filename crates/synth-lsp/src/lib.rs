//! A language server for Weft.
//!
//! Speaks the Language Server Protocol over stdio: diagnostics, hover,
//! go to definition, references, highlights, rename, the outline, completion
//! and semantic tokens. Each file is analyzed together with the other
//! `.weft` files of its directory (open editor buffers win over the disk),
//! and with the Verilog files there when a Weft file declares an
//! `extern module`.

pub mod rpc;
mod symbols;
pub mod text;

use std::collections::{BTreeSet, HashMap};
use std::io::{self, BufRead, Write};
use std::path::Path;

use synth_core::{Diagnostic, FileId, Json, Severity, SourceMap};
use synth_weft::RefKind;

use text::{LineIndex, path_to_uri, uri_to_path};

/// A resolved name in a file, by byte offsets.
#[derive(Clone, Debug)]
struct Ref {
    start: usize,
    end: usize,
    decl: Option<(String, usize, usize)>,
    kind: RefKind,
    hover: String,
    members: Vec<String>,
}

/// The index of a file as of its last analysis without syntax errors.
#[derive(Debug, Default)]
struct Snapshot {
    text: String,
    refs: Vec<Ref>,
}

/// Language server state; transport-independent.
#[derive(Debug)]
pub struct Server {
    preludes: Vec<(String, String)>,
    docs: HashMap<String, String>,
    snapshots: HashMap<String, Snapshot>,
    dirty: BTreeSet<String>,
    utf8: bool,
    shutdown: bool,
    /// Set by `exit`, with the process exit code.
    pub exit: Option<i32>,
}

const TOKEN_TYPES: &[&str] = &[
    "class",
    "function",
    "type",
    "enumMember",
    "property",
    "parameter",
    "variable",
    "namespace",
    "event",
    "typeParameter",
];
const TOKEN_MODIFIERS: &[&str] = &["declaration", "readonly"];

impl Server {
    /// `preludes` are `(name, source)` pairs of extern declarations, as
    /// given to [`synth_weft::compile`].
    pub fn new(preludes: Vec<(String, String)>) -> Server {
        Server {
            preludes,
            docs: HashMap::new(),
            snapshots: HashMap::new(),
            dirty: BTreeSet::new(),
            utf8: false,
            shutdown: false,
            exit: None,
        }
    }

    /// Handles one message and returns the messages to send.
    pub fn handle(&mut self, msg: &Json) -> Vec<Json> {
        let method = msg.at(&["method"]).as_str().unwrap_or("");
        let params = msg.at(&["params"]);
        let id = msg.get("id");
        let mut out = Vec::new();
        if let Some(id) = id
            && msg.get("method").is_some()
        {
            if self.shutdown && method != "exit" {
                out.push(rpc::error(id, -32600, "the server is shutting down"));
                return out;
            }
            // Requests about a file see its latest text.
            if let Some(uri) = params.at(&["textDocument", "uri"]).as_str()
                && self.dirty.contains(uri)
            {
                let uri = uri.to_string();
                out.extend(self.analyze(&uri));
            }
            let result = match method {
                "initialize" => Ok(self.initialize(params)),
                "shutdown" => {
                    self.shutdown = true;
                    Ok(Json::Null)
                }
                "textDocument/hover" => Ok(self.hover(params)),
                "textDocument/definition" => Ok(self.definition(params)),
                "textDocument/references" => Ok(self.references(params)),
                "textDocument/documentHighlight" => Ok(self.highlights(params)),
                "textDocument/documentSymbol" => Ok(self.document_symbols(params)),
                "textDocument/completion" => Ok(self.completion(params)),
                "textDocument/prepareRename" => Ok(self.prepare_rename(params)),
                "textDocument/rename" => self.rename(params),
                "textDocument/semanticTokens/full" => Ok(self.semantic_tokens(params)),
                "textDocument/formatting" => Ok(self.formatting(params)),
                _ => Err((-32601, format!("unsupported method `{method}`"))),
            };
            out.push(match result {
                Ok(r) => rpc::response(id, r),
                Err((code, m)) => rpc::error(id, code, &m),
            });
            return out;
        }
        match method {
            "exit" => self.exit = Some(if self.shutdown { 0 } else { 1 }),
            "textDocument/didOpen" => {
                let uri = params.at(&["textDocument", "uri"]).as_str().unwrap_or("").to_string();
                let text = params.at(&["textDocument", "text"]).as_str().unwrap_or("").to_string();
                self.docs.insert(uri.clone(), text);
                self.dirty.insert(uri);
            }
            "textDocument/didChange" => {
                let uri = params.at(&["textDocument", "uri"]).as_str().unwrap_or("").to_string();
                // Full synchronization: the last change holds the whole text.
                if let Some(c) = params.at(&["contentChanges"]).as_array().last()
                    && let Some(t) = c.at(&["text"]).as_str()
                {
                    self.docs.insert(uri.clone(), t.to_string());
                    self.dirty.insert(uri);
                }
            }
            "textDocument/didSave" => {
                if let Some(uri) = params.at(&["textDocument", "uri"]).as_str() {
                    self.dirty.insert(uri.to_string());
                }
            }
            "textDocument/didClose" => {
                if let Some(uri) = params.at(&["textDocument", "uri"]).as_str() {
                    self.docs.remove(uri);
                    // Keep the diagnostics of files that still exist.
                    if uri_to_path(uri).is_none_or(|p| !Path::new(&p).exists()) {
                        self.snapshots.remove(uri);
                        out.push(publish(uri, Vec::new()));
                    }
                }
            }
            _ => {}
        }
        out
    }

    /// Analyzes the files changed since the last call.
    pub fn flush(&mut self) -> Vec<Json> {
        let mut out = Vec::new();
        while let Some(uri) = self.dirty.pop_first() {
            out.extend(self.analyze(&uri));
        }
        out
    }

    fn initialize(&mut self, params: &Json) -> Json {
        let encs = params.at(&["capabilities", "general", "positionEncodings"]).as_array();
        self.utf8 = encs.iter().any(|e| e.as_str() == Some("utf-8"));
        let caps = Json::obj()
            .with("positionEncoding", if self.utf8 { "utf-8" } else { "utf-16" })
            .with(
                "textDocumentSync",
                Json::obj().with("openClose", true).with("change", 1).with("save", true),
            )
            .with("hoverProvider", true)
            .with("definitionProvider", true)
            .with("referencesProvider", true)
            .with("documentHighlightProvider", true)
            .with("documentSymbolProvider", true)
            .with("documentFormattingProvider", true)
            .with("renameProvider", Json::obj().with("prepareProvider", true))
            .with("completionProvider", Json::obj().with("triggerCharacters", vec!["."]))
            .with(
                "semanticTokensProvider",
                Json::obj()
                    .with(
                        "legend",
                        Json::obj()
                            .with("tokenTypes", TOKEN_TYPES.to_vec())
                            .with("tokenModifiers", TOKEN_MODIFIERS.to_vec()),
                    )
                    .with("full", true),
            );
        Json::obj().with("capabilities", caps).with(
            "serverInfo",
            Json::obj().with("name", "synth-lsp").with("version", env!("CARGO_PKG_VERSION")),
        )
    }

    fn text_of(&self, uri: &str) -> Option<String> {
        if let Some(t) = self.docs.get(uri) {
            return Some(t.clone());
        }
        std::fs::read_to_string(uri_to_path(uri)?).ok()
    }

    /// The files analyzed with `uri`: itself, and the Weft files of its
    /// directory that define names it uses (transitively), leaving out
    /// files whose definitions clash with ones already in the group (other
    /// designs sharing the directory).
    fn group(&self, uri: &str) -> Vec<(String, String)> {
        let mut uris: BTreeSet<String> = BTreeSet::new();
        if let Some(path) = uri_to_path(uri)
            && let Some(dir) = Path::new(&path).parent()
        {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.extension().is_some_and(|x| x == "weft")
                        && let Some(s) = p.to_str()
                    {
                        uris.insert(path_to_uri(s));
                    }
                }
            }
            for u in self.docs.keys() {
                if u.ends_with(".weft")
                    && uri_to_path(u).is_some_and(|p| Path::new(&p).parent() == Some(dir))
                {
                    uris.insert(u.clone());
                }
            }
        }
        uris.remove(uri);
        let Some(text) = self.text_of(uri) else { return Vec::new() };
        let mut others: Vec<(String, String, Names)> = uris
            .into_iter()
            .filter_map(|u| self.text_of(&u).map(|t| (u, t)))
            .map(|(u, t)| {
                let n = names(&t);
                (u, t, n)
            })
            .collect();
        let me = names(&text);
        let mut defined = me.defs.clone();
        let mut used = me.uses;
        let mut group = vec![(uri.to_string(), text)];
        loop {
            let pick = others.iter().position(|(_, _, n)| {
                n.defs.iter().any(|d| used.contains(d) && !defined.contains(d))
                    && n.defs.iter().all(|d| !defined.contains(d))
            });
            let Some(i) = pick else { break };
            let (u, t, n) = others.remove(i);
            defined.extend(n.defs);
            used.extend(n.uses);
            group.push((u, t));
        }
        group
    }

    /// Verilog files next to `uri`, for extern modules.
    fn verilog_of(&self, uri: &str) -> Vec<(String, String)> {
        let Some(path) = uri_to_path(uri) else { return Vec::new() };
        let Some(dir) = Path::new(&path).parent() else { return Vec::new() };
        let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
        let mut out: Vec<(String, String)> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "v" || x == "sv"))
            .filter_map(|p| {
                let s = p.to_str()?.to_string();
                let t = std::fs::read_to_string(&p).ok()?;
                Some((s, t))
            })
            .collect();
        out.sort();
        out
    }

    fn analyze(&mut self, uri: &str) -> Vec<Json> {
        let files = self.group(uri);
        let mut sm = SourceMap::new();
        let mut ids = Vec::new();
        let mut uri_of: HashMap<FileId, String> = HashMap::new();
        for (u, t) in &files {
            let name = uri_to_path(u).unwrap_or_else(|| u.clone());
            let id = sm.add(name, t.clone());
            ids.push(id);
            uri_of.insert(id, u.clone());
        }
        let verilog = if files.iter().any(|(_, t)| t.contains("extern module")) {
            let vs = self.verilog_of(uri);
            let vids: Vec<FileId> = vs.iter().map(|(p, t)| sm.add(p.clone(), t.clone())).collect();
            if vids.is_empty() {
                None
            } else {
                let (src, d) = synth_verilog::parse_files(&mut sm, &vids, &Default::default());
                if d.has_errors() { None } else { Some(src) }
            }
        } else {
            None
        };
        let preludes: Vec<(&str, &str)> =
            self.preludes.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let opts = synth_weft::Options::default();
        let a = synth_weft::analyze(&mut sm, &ids, &preludes, verilog.as_ref(), &opts);
        for (u, _) in &files {
            self.dirty.remove(u);
        }

        let mut by_uri: HashMap<String, Vec<Json>> =
            files.iter().map(|(u, _)| (u.clone(), Vec::new())).collect();
        let indexes: HashMap<FileId, LineIndex> =
            ids.iter().map(|&id| (id, LineIndex::new(&sm.file(id).text, self.utf8))).collect();
        for dg in &a.diags.items {
            let (target, start, end) = match dg.span {
                Some(sp) if uri_of.contains_key(&sp.file) => {
                    (uri_of[&sp.file].clone(), sp.start as usize, sp.end as usize)
                }
                Some(_) => continue,
                None => (uri.to_string(), 0, 0),
            };
            let fid = uri_of.iter().find(|(_, u)| **u == target).map(|(f, _)| *f).unwrap();
            let lsp = diagnostic(dg, &indexes[&fid], start, end, &uri_of, &indexes);
            by_uri.entry(target).or_default().push(lsp);
        }

        // Update the index of every file of the group (unless the syntax
        // was broken, in which case the analysis stopped early).
        if a.parsed {
            let mut refs: HashMap<FileId, Vec<Ref>> = HashMap::new();
            for r in &a.refs {
                if !uri_of.contains_key(&r.span.file) {
                    continue;
                }
                let decl = r.decl.and_then(|d| {
                    uri_of.get(&d.file).map(|u| (u.clone(), d.start as usize, d.end as usize))
                });
                refs.entry(r.span.file).or_default().push(Ref {
                    start: r.span.start as usize,
                    end: r.span.end as usize,
                    decl,
                    kind: r.kind,
                    hover: r.hover.clone(),
                    members: r.members.clone(),
                });
            }
            for &id in &ids {
                let mut rs = refs.remove(&id).unwrap_or_default();
                rs.sort_by_key(|r| (r.start, r.end));
                let snap = Snapshot { text: sm.file(id).text.clone(), refs: rs };
                self.snapshots.insert(uri_of[&id].clone(), snap);
            }
        }
        let mut uris: Vec<&String> = by_uri.keys().collect();
        uris.sort();
        uris.into_iter().map(|u| publish(u, by_uri[u].clone())).collect()
    }

    /// The fresh index of a file and the byte offset of a position.
    fn locate<'s>(&'s self, params: &'s Json) -> Option<(&'s str, &'s Snapshot, usize)> {
        let uri = params.at(&["textDocument", "uri"]).as_str()?;
        let snap = self.snapshots.get(uri)?;
        let current = self.text_of(uri)?;
        if current != snap.text {
            return None;
        }
        let off = LineIndex::new(&snap.text, self.utf8).offset(params.at(&["position"]));
        Some((uri, snap, off))
    }

    fn ref_at(snap: &Snapshot, off: usize) -> Option<&Ref> {
        // Prefer a name under the cursor, then one just before it.
        snap.refs
            .iter()
            .filter(|r| r.start <= off && off < r.end)
            .min_by_key(|r| r.end - r.start)
            .or_else(|| snap.refs.iter().find(|r| r.end == off))
    }

    fn location(&self, uri: &str, start: usize, end: usize) -> Option<Json> {
        let text = self.snapshots.get(uri).map(|s| s.text.clone()).or_else(|| self.text_of(uri))?;
        let ix = LineIndex::new(&text, self.utf8);
        Some(Json::obj().with("uri", uri).with("range", ix.range(start, end)))
    }

    fn hover(&self, params: &Json) -> Json {
        let Some((_, snap, off)) = self.locate(params) else { return Json::Null };
        let Some(r) = Self::ref_at(snap, off) else { return Json::Null };
        let ix = LineIndex::new(&snap.text, self.utf8);
        Json::obj()
            .with("contents", Json::obj().with("kind", "markdown").with("value", r.hover.as_str()))
            .with("range", ix.range(r.start, r.end))
    }

    fn definition(&self, params: &Json) -> Json {
        let Some((_, snap, off)) = self.locate(params) else { return Json::Null };
        let Some(Ref { decl: Some((u, s, e)), .. }) = Self::ref_at(snap, off) else {
            return Json::Null;
        };
        self.location(u, *s, *e).unwrap_or_default()
    }

    /// Every use of the declaration under the cursor, by file.
    fn occurrences(&self, params: &Json) -> Option<Vec<(String, usize, usize)>> {
        let (_, snap, off) = self.locate(params)?;
        let target = Self::ref_at(snap, off)?.decl.clone()?;
        let mut out = Vec::new();
        let mut uris: Vec<&String> = self.snapshots.keys().collect();
        uris.sort();
        for u in uris {
            for r in &self.snapshots[u].refs {
                if r.decl.as_ref() == Some(&target) {
                    out.push((u.clone(), r.start, r.end));
                }
            }
        }
        if !out.contains(&target) {
            out.insert(0, target);
        }
        Some(out)
    }

    fn references(&self, params: &Json) -> Json {
        let with_decl = params.at(&["context", "includeDeclaration"]).as_bool().unwrap_or(true);
        let Some(occ) = self.occurrences(params) else { return Json::Null };
        let decl = self
            .locate(params)
            .and_then(|(_, s, o)| Self::ref_at(s, o).and_then(|r| r.decl.clone()));
        Json::Arr(
            occ.into_iter()
                .filter(|o| with_decl || Some(o) != decl.as_ref())
                .filter_map(|(u, s, e)| self.location(&u, s, e))
                .collect(),
        )
    }

    fn highlights(&self, params: &Json) -> Json {
        let Some((uri, snap, _)) = self.locate(params) else { return Json::Null };
        let Some(occ) = self.occurrences(params) else { return Json::Null };
        let ix = LineIndex::new(&snap.text, self.utf8);
        Json::Arr(
            occ.into_iter()
                .filter(|(u, _, _)| u == uri)
                .map(|(_, s, e)| Json::obj().with("range", ix.range(s, e)))
                .collect(),
        )
    }

    fn prepare_rename(&self, params: &Json) -> Json {
        let Some((_, snap, off)) = self.locate(params) else { return Json::Null };
        match Self::ref_at(snap, off) {
            Some(r) if r.decl.is_some() => {
                let ix = LineIndex::new(&snap.text, self.utf8);
                ix.range(r.start, r.end)
            }
            _ => Json::Null,
        }
    }

    fn rename(&self, params: &Json) -> Result<Json, (i64, String)> {
        let name = params.at(&["newName"]).as_str().unwrap_or("");
        let valid = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !synth_weft::parser::KEYWORDS.contains(&name);
        if !valid {
            return Err((-32602, format!("`{name}` is not a valid name")));
        }
        let Some(occ) = self.occurrences(params) else {
            return Err((-32602, "nothing to rename here".into()));
        };
        let mut changes = Json::obj();
        for (u, s, e) in occ {
            let Some(text) = self.snapshots.get(&u).map(|s| &s.text) else { continue };
            let ix = LineIndex::new(text, self.utf8);
            let edit = Json::obj().with("range", ix.range(s, e)).with("newText", name);
            match changes.get(&u) {
                Some(_) => {
                    if let Json::Obj(m) = &mut changes
                        && let Some((_, Json::Arr(a))) = m.iter_mut().find(|(k, _)| *k == u)
                    {
                        a.push(edit);
                    }
                }
                None => changes.set(&u, vec![edit]),
            }
        }
        Ok(Json::obj().with("changes", changes))
    }

    fn document_symbols(&self, params: &Json) -> Json {
        let Some(uri) = params.at(&["textDocument", "uri"]).as_str() else { return Json::Null };
        let Some(text) = self.text_of(uri) else { return Json::Null };
        let mut sm = SourceMap::new();
        let id = sm.add(uri, text.clone());
        let (file, _) = synth_weft::parse_file(&sm, id);
        symbols::document_symbols(&file, &text, &LineIndex::new(&text, self.utf8))
    }

    fn completion(&self, params: &Json) -> Json {
        let Some(uri) = params.at(&["textDocument", "uri"]).as_str() else { return Json::Null };
        let Some(text) = self.text_of(uri) else { return Json::Null };
        let off = LineIndex::new(&text, self.utf8).offset(params.at(&["position"]));
        let snap = self.snapshots.get(uri);
        let items = complete(&text, off, snap, &self.snapshots);
        Json::obj().with("isIncomplete", false).with("items", items)
    }

    fn formatting(&self, params: &Json) -> Json {
        let Some(uri) = params.at(&["textDocument", "uri"]).as_str() else { return Json::Null };
        let Some(text) = self.text_of(uri) else { return Json::Null };
        let out = synth_weft::fmt::format(&text);
        if out == text {
            return Json::Arr(Vec::new());
        }
        let ix = LineIndex::new(&text, self.utf8);
        Json::Arr(vec![Json::obj().with("range", ix.range(0, text.len())).with("newText", out)])
    }

    fn semantic_tokens(&self, params: &Json) -> Json {
        let Some(uri) = params.at(&["textDocument", "uri"]).as_str() else { return Json::Null };
        let Some(snap) = self.snapshots.get(uri) else { return Json::Null };
        if self.text_of(uri).as_deref() != Some(snap.text.as_str()) {
            return Json::Null;
        }
        let ix = LineIndex::new(&snap.text, self.utf8);
        let mut data: Vec<Json> = Vec::new();
        let (mut pl, mut pc) = (0i64, 0i64);
        let mut last_end = 0;
        for r in &snap.refs {
            if r.start < last_end || snap.text[r.start..r.end].contains('\n') {
                continue;
            }
            last_end = r.end;
            let (ty, mut mods) = match r.kind {
                RefKind::Module => (0, 0),
                RefKind::Function => (1, 0),
                RefKind::Type => (2, 0),
                RefKind::Variant => (3, 2),
                RefKind::Field | RefKind::Register => (4, 0),
                RefKind::Port => (5, 0),
                RefKind::Wire | RefKind::Memory | RefKind::Variable => (6, 0),
                RefKind::Const | RefKind::Value => (6, 2),
                RefKind::Instance => (7, 0),
                RefKind::Domain => (8, 0),
                RefKind::Generic => (9, 2),
            };
            if r.decl.as_ref().is_some_and(|(u, s, _)| u == uri && *s == r.start) {
                mods |= 1;
            }
            let p = ix.position(r.start);
            let q = ix.position(r.end);
            let line = p.at(&["line"]).as_i64().unwrap_or(0);
            let col = p.at(&["character"]).as_i64().unwrap_or(0);
            let len = q.at(&["character"]).as_i64().unwrap_or(0) - col;
            let dc = if line == pl { col - pc } else { col };
            data.extend([line - pl, dc, len, ty, mods].map(Json::from));
            (pl, pc) = (line, col);
        }
        Json::obj().with("data", data)
    }
}

/// Top-level names a file defines and identifiers it mentions.
#[derive(Debug)]
struct Names {
    defs: BTreeSet<String>,
    uses: BTreeSet<String>,
}

fn names(text: &str) -> Names {
    let mut sm = SourceMap::new();
    let id = sm.add("x", text);
    let (file, _) = synth_weft::parse_file(&sm, id);
    let defs = file
        .items
        .iter()
        .filter_map(|it| match &it.kind {
            synth_weft::ast::ItemKind::Module(m) => Some(m.name.name.clone()),
            synth_weft::ast::ItemKind::Function(f) => Some(f.name.name.clone()),
            synth_weft::ast::ItemKind::Struct(s) => Some(s.name.name.clone()),
            synth_weft::ast::ItemKind::Enum(e) => Some(e.name.name.clone()),
            synth_weft::ast::ItemKind::Const(c) => Some(c.name.name.clone()),
            synth_weft::ast::ItemKind::Assert(_) => None,
        })
        .collect();
    let mut diags = synth_core::Diagnostics::default();
    let uses = synth_weft::lexer::lex(id, text, &mut diags)
        .into_iter()
        .filter_map(|t| match t.tok {
            synth_weft::lexer::Tok::Ident(n) => Some(n),
            _ => None,
        })
        .collect();
    Names { defs, uses }
}

fn publish(uri: &str, diags: Vec<Json>) -> Json {
    rpc::notification(
        "textDocument/publishDiagnostics",
        Json::obj().with("uri", uri).with("diagnostics", diags),
    )
}

fn diagnostic(
    dg: &Diagnostic,
    ix: &LineIndex,
    start: usize,
    end: usize,
    uri_of: &HashMap<FileId, String>,
    indexes: &HashMap<FileId, LineIndex>,
) -> Json {
    let mut message = dg.message.clone();
    for (sp, n) in &dg.notes {
        if sp.is_none() {
            message.push_str(&format!("\nnote: {n}"));
        }
    }
    for h in &dg.helps {
        message.push_str(&format!("\nhelp: {h}"));
    }
    let severity = match dg.severity {
        Severity::Error => 1,
        Severity::Warning => 2,
        Severity::Note => 3,
    };
    let mut related = Vec::new();
    let spans =
        dg.labels.iter().map(|(s, m)| (Some(*s), m)).chain(dg.notes.iter().map(|(s, m)| (*s, m)));
    for (sp, m) in spans {
        let Some(sp) = sp else { continue };
        let (Some(u), Some(lix)) = (uri_of.get(&sp.file), indexes.get(&sp.file)) else { continue };
        related.push(
            Json::obj()
                .with(
                    "location",
                    Json::obj()
                        .with("uri", u.as_str())
                        .with("range", lix.range(sp.start as usize, sp.end as usize)),
                )
                .with("message", m.as_str()),
        );
    }
    let mut d = Json::obj()
        .with("range", ix.range(start, end))
        .with("severity", severity)
        .with("source", "weft")
        .with("message", message);
    if !related.is_empty() {
        d.set("relatedInformation", related);
    }
    if let Some(c) = lint_code(&dg.message) {
        d.set("code", c);
    }
    d
}

/// Lint warnings name their rule in brackets at the end.
fn lint_code(message: &str) -> Option<&str> {
    let m = message.strip_suffix(']')?;
    let i = m.rfind('[')?;
    let code = &m[i + 1..];
    code.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c == '-').then_some(code)
}

const BUILTIN_TYPES: &[&str] = &["bool", "uint", "sint", "index", "clock", "int", "string"];

fn completion_kind(k: RefKind) -> u32 {
    match k {
        RefKind::Module => 9,
        RefKind::Function => 3,
        RefKind::Type => 22,
        RefKind::Variant => 20,
        RefKind::Field => 5,
        RefKind::Port => 10,
        RefKind::Instance => 7,
        RefKind::Domain => 23,
        RefKind::Const => 21,
        RefKind::Generic => 25,
        RefKind::Wire
        | RefKind::Register
        | RefKind::Memory
        | RefKind::Value
        | RefKind::Variable => 6,
    }
}

/// The first line of a hover's code block.
fn signature(hover: &str) -> String {
    hover.strip_prefix("```weft\n").and_then(|s| s.lines().next()).unwrap_or("").to_string()
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn complete(
    text: &str,
    off: usize,
    snap: Option<&Snapshot>,
    all: &HashMap<String, Snapshot>,
) -> Vec<Json> {
    let b = text.as_bytes();
    let off = off.min(b.len());
    let mut start = off;
    while start > 0 && is_ident(b[start - 1]) {
        start -= 1;
    }
    let item = |label: &str, kind: u32, detail: &str| {
        let mut j = Json::obj().with("label", label).with("kind", kind);
        if !detail.is_empty() {
            j.set("detail", detail);
        }
        j
    };
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    // Member access: `x.`.
    if start > 0 && b[start - 1] == b'.' {
        let mut s = start - 1;
        while s > 0 && is_ident(b[s - 1]) {
            s -= 1;
        }
        let base = &text[s..start - 1];
        let Some(snap) = snap else { return out };
        if base.is_empty() {
            // `.Variant` in a typed context: every variant.
            for r in snap.refs.iter().filter(|r| r.kind == RefKind::Variant) {
                let n = &snap.text[r.start..r.end];
                if seen.insert(n.to_string()) {
                    out.push(item(n, 20, &signature(&r.hover)));
                }
            }
            return out;
        }
        // The nearest earlier reference with that name.
        let shifted = shift(off, text, &snap.text);
        let r = snap
            .refs
            .iter()
            .filter(|r| &snap.text[r.start..r.end] == base && !r.members.is_empty())
            .min_by_key(|r| (r.start > shifted, r.start.abs_diff(shifted)));
        if let Some(r) = r {
            let kind = if r.kind == RefKind::Type { 20 } else { 5 };
            for m in &r.members {
                if seen.insert(m.clone()) {
                    out.push(item(m, kind, ""));
                }
            }
        }
        return out;
    }
    // Names declared in the enclosing module, then everywhere visible.
    if let Some(snap) = snap {
        let cursor = shift(off, text, &snap.text);
        let module = enclosing_module(&snap.text, cursor);
        for r in &snap.refs {
            let is_decl = r.decl.as_ref().is_some_and(|(_, s, _)| *s == r.start);
            let local = module.is_some_and(|(a, z)| a <= r.start && r.start < z);
            let global = matches!(r.kind, RefKind::Module | RefKind::Function | RefKind::Type)
                || (r.kind == RefKind::Const && enclosing_module(&snap.text, r.start).is_none());
            if is_decl && (local || global) {
                let n = &snap.text[r.start..r.end];
                if seen.insert(n.to_string()) {
                    out.push(item(n, completion_kind(r.kind), &signature(&r.hover)));
                }
            }
        }
    }
    let mut uris: Vec<&String> = all.keys().collect();
    uris.sort();
    for u in uris {
        let s = &all[u];
        for r in &s.refs {
            let is_decl = r.decl.as_ref().is_some_and(|(du, ds, _)| du == u && *ds == r.start);
            if is_decl && matches!(r.kind, RefKind::Module | RefKind::Function | RefKind::Type) {
                let n = &s.text[r.start..r.end];
                if seen.insert(n.to_string()) {
                    out.push(item(n, completion_kind(r.kind), &signature(&r.hover)));
                }
            }
        }
    }
    let std_items = synth_weft::STD.lines().filter_map(|l| {
        l.strip_prefix("module ").map(|r| (r, 9)).or_else(|| l.strip_prefix("fn ").map(|r| (r, 3)))
    });
    for (rest, kind) in std_items {
        let n: String =
            rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if seen.insert(n.clone()) {
            out.push(item(&n, kind, "standard library"));
        }
    }
    for f in synth_weft::BUILTINS {
        if seen.insert(f.to_string()) {
            out.push(item(f, 3, "builtin"));
        }
    }
    for t in BUILTIN_TYPES {
        if seen.insert(t.to_string()) {
            out.push(item(t, 22, "builtin type"));
        }
    }
    for k in synth_weft::parser::KEYWORDS {
        if seen.insert(k.to_string()) {
            out.push(item(k, 14, ""));
        }
    }
    out
}

/// Maps an offset in `now` to the matching offset in `old`, assuming one
/// edited region.
fn shift(off: usize, now: &str, old: &str) -> usize {
    let common = now.bytes().zip(old.bytes()).take_while(|(a, b)| a == b).count();
    if off <= common {
        off
    } else {
        (off as isize + old.len() as isize - now.len() as isize)
            .clamp(common as isize, old.len() as isize) as usize
    }
}

/// The byte range of the module definition containing `off`.
fn enclosing_module(text: &str, off: usize) -> Option<(usize, usize)> {
    let mut sm = SourceMap::new();
    let id = sm.add("x", text);
    let (file, _) = synth_weft::parse_file(&sm, id);
    file.items.iter().find_map(|it| match &it.kind {
        synth_weft::ast::ItemKind::Module(_) | synth_weft::ast::ItemKind::Function(_)
            if (it.span.start as usize) <= off && off <= it.span.end as usize =>
        {
            Some((it.span.start as usize, it.span.end as usize))
        }
        _ => None,
    })
}

/// Runs the server on stdin and stdout until `exit`.
pub fn serve(preludes: Vec<(String, String)>) -> io::Result<i32> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        while let Ok(Some(m)) = rpc::read(&mut input) {
            if tx.send(m).is_err() {
                break;
            }
        }
    });
    let mut server = Server::new(preludes);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    while let Ok(first) = rx.recv() {
        // Handle everything already queued before analyzing.
        let mut batch = vec![first];
        batch.extend(rx.try_iter());
        for m in &batch {
            for o in server.handle(m) {
                rpc::write(&mut out, &o)?;
            }
            if let Some(code) = server.exit {
                return Ok(code);
            }
        }
        for o in server.flush() {
            rpc::write(&mut out, &o)?;
        }
    }
    Ok(if server.shutdown { 0 } else { 1 })
}

/// Reads messages from `input` and answers them on `output` without a
/// reader thread (for tests and embedding).
pub fn serve_on(
    server: &mut Server,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<()> {
    while let Some(m) = rpc::read(input)? {
        for o in server.handle(&m) {
            rpc::write(output, &o)?;
        }
        if server.exit.is_some() {
            break;
        }
        for o in server.flush() {
            rpc::write(output, &o)?;
        }
    }
    Ok(())
}
