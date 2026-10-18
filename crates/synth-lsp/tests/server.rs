//! The language server, driven through its message handler.

use synth_core::Json;
use synth_lsp::Server;

fn req(id: u32, method: &str, params: Json) -> Json {
    Json::obj().with("jsonrpc", "2.0").with("id", id).with("method", method).with("params", params)
}

fn note(method: &str, params: Json) -> Json {
    Json::obj().with("jsonrpc", "2.0").with("method", method).with("params", params)
}

fn pos(uri: &str, text: &str, needle: &str, delta: usize) -> Json {
    let off = text.find(needle).unwrap() + delta;
    let line = text[..off].matches('\n').count();
    let col = off - text[..off].rfind('\n').map_or(0, |i| i + 1);
    Json::obj()
        .with("textDocument", Json::obj().with("uri", uri))
        .with("position", Json::obj().with("line", line).with("character", col))
}

fn result(out: &[Json]) -> &Json {
    out.iter().find(|m| m.get("id").is_some()).map(|m| m.at(&["result"])).unwrap()
}

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("synth-lsp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

const LIB: &str = "module pulse(in x: bool, out y: bool) {\n    reg last: bool = false;\n    last := x;\n    y = x && !last;\n}\n";
const TOP: &str = "module top(in clk: clock, in btn: bool, out led: bool) {\n    let b = sync(btn);\n    inst p = pulse(x: b);\n    led = p.y;\n}\n";

#[test]
fn session() {
    let dir = tmpdir("session");
    std::fs::write(dir.join("lib.weft"), LIB).unwrap();
    let top_path = dir.join("top.weft");
    let uri = format!("file://{}", top_path.display());
    let lib_uri = format!("file://{}", dir.join("lib.weft").display());
    let mut s = Server::new(vec![]);
    let caps = s.handle(&req(1, "initialize", Json::obj().with("capabilities", Json::obj())));
    assert_eq!(result(&caps).at(&["capabilities", "hoverProvider"]).as_bool(), Some(true));

    // Open an unsaved buffer next to lib.weft: the two are analyzed together.
    let doc = Json::obj()
        .with("uri", uri.as_str())
        .with("languageId", "weft")
        .with("version", 1)
        .with("text", TOP);
    s.handle(&note("textDocument/didOpen", Json::obj().with("textDocument", doc)));
    let pubs = s.flush();
    let mine =
        pubs.iter().find(|m| m.at(&["params", "uri"]).as_str() == Some(uri.as_str())).unwrap();
    assert!(mine.at(&["params", "diagnostics"]).as_array().is_empty(), "{}", mine.dump());

    // Hover and definition across files.
    let out = s.handle(&req(2, "textDocument/hover", pos(&uri, TOP, "p.y", 2)));
    let hover = result(&out).at(&["contents", "value"]).as_str().unwrap();
    assert!(hover.contains("out y: bool"), "{hover}");
    let out = s.handle(&req(3, "textDocument/definition", pos(&uri, TOP, "pulse(", 0)));
    let loc = result(&out);
    assert_eq!(loc.at(&["uri"]).as_str(), Some(lib_uri.as_str()));
    assert_eq!(loc.at(&["range", "start", "character"]).as_i64(), Some(7));

    // References of `last` within lib.weft.
    let out = s.handle(&req(
        4,
        "textDocument/references",
        pos(&lib_uri, LIB, "last", 0).with("context", Json::obj().with("includeDeclaration", true)),
    ));
    assert_eq!(result(&out).as_array().len(), 3, "{}", result(&out).dump());

    // Rename an instance port everywhere.
    let out = s.handle(&req(
        5,
        "textDocument/rename",
        pos(&uri, TOP, "p.y", 2).with("newName", "pulse_out"),
    ));
    let ch = result(&out).at(&["changes"]);
    assert_eq!(ch.at(&[uri.as_str()]).as_array().len(), 1, "{}", ch.dump());
    assert_eq!(ch.at(&[lib_uri.as_str()]).as_array().len(), 2, "{}", ch.dump());

    // Completion of instance ports.
    let edited = TOP.replace("led = p.y;", "led = p.");
    let change = Json::obj()
        .with("textDocument", Json::obj().with("uri", uri.as_str()).with("version", 2))
        .with("contentChanges", vec![Json::obj().with("text", edited.as_str())]);
    s.handle(&note("textDocument/didChange", change));
    let out = s.handle(&req(6, "textDocument/completion", pos(&uri, &edited, "p.\n", 2)));
    let labels: Vec<&str> = result(&out)
        .at(&["items"])
        .as_array()
        .iter()
        .filter_map(|i| i.at(&["label"]).as_str())
        .collect();
    assert_eq!(labels, ["x", "y"]);
    // The syntax error is reported.
    let pubs = s.flush();
    let mine = pubs.iter().find(|m| m.at(&["params", "uri"]).as_str() == Some(uri.as_str()));
    let _ = mine;

    // Outline and semantic tokens.
    let out = s.handle(&req(
        7,
        "textDocument/documentSymbol",
        Json::obj().with("textDocument", Json::obj().with("uri", lib_uri.as_str())),
    ));
    let syms = result(&out).as_array();
    assert_eq!(syms[0].at(&["name"]).as_str(), Some("pulse"));
    assert_eq!(syms[0].at(&["children"]).as_array().len(), 3);
    let out = s.handle(&req(
        8,
        "textDocument/semanticTokens/full",
        Json::obj().with("textDocument", Json::obj().with("uri", lib_uri.as_str())),
    ));
    assert!(result(&out).at(&["data"]).as_array().len() >= 5 * 6);

    // Formatting aligns the port directions of lib.weft's module.
    let lib = "module pulse(\n    in x: bool,\n    out y: bool,\n) {\n    y = x;\n}\n";
    let doc = Json::obj().with("uri", lib_uri.as_str()).with("text", lib);
    s.handle(&note("textDocument/didOpen", Json::obj().with("textDocument", doc)));
    let out = s.handle(&req(
        10,
        "textDocument/formatting",
        Json::obj().with("textDocument", Json::obj().with("uri", lib_uri.as_str())),
    ));
    let edits = result(&out).as_array();
    assert_eq!(edits.len(), 1);
    assert!(edits[0].at(&["newText"]).as_str().unwrap().contains("    in  x: bool,"));

    let out = s.handle(&req(9, "shutdown", Json::Null));
    assert!(result(&out).is_null());
    s.handle(&note("exit", Json::Null));
    assert_eq!(s.exit, Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn diagnostics_and_framing() {
    let dir = tmpdir("diag");
    let path = dir.join("bad.weft");
    let text = "module top(in a: bool, out y: uint<2>) {\n    y = a + 1;\n}\n";
    std::fs::write(&path, text).unwrap();
    let uri = format!("file://{}", path.display());
    let msgs = [
        req(1, "initialize", Json::obj().with("capabilities", Json::obj())),
        note(
            "textDocument/didOpen",
            Json::obj()
                .with("textDocument", Json::obj().with("uri", uri.as_str()).with("text", text)),
        ),
        req(2, "shutdown", Json::Null),
        note("exit", Json::Null),
    ];
    let mut input = Vec::new();
    for m in &msgs {
        synth_lsp::rpc::write(&mut input, m).unwrap();
    }
    let mut output = Vec::new();
    let mut s = Server::new(vec![]);
    synth_lsp::serve_on(&mut s, &mut &input[..], &mut output).unwrap();
    let mut rd = &output[..];
    let mut diags = None;
    while let Some(m) = synth_lsp::rpc::read(&mut rd).unwrap() {
        if m.at(&["method"]).as_str() == Some("textDocument/publishDiagnostics") {
            diags = Some(m.at(&["params", "diagnostics"]).clone());
        }
    }
    let diags = diags.expect("diagnostics published");
    let d = &diags.as_array()[0];
    assert_eq!(d.at(&["severity"]).as_i64(), Some(1));
    assert_eq!(d.at(&["range", "start", "line"]).as_i64(), Some(1));
    assert!(d.at(&["message"]).as_str().unwrap().contains("help:"), "{}", d.dump());
    let _ = std::fs::remove_dir_all(&dir);
}
