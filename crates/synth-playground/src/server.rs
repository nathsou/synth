//! A minimal HTTP server for the playground: the web app's files, and its
//! requests under `/api/<method>` answered natively instead of by the
//! WebAssembly build (`/api/<method>?progress` streams the progress of the
//! run before its result). With `SYNTH_PLAYGROUND_DIR` set, files are read from
//! that directory instead of the embedded copies (for working on the app).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

/// The web app, embedded.
pub const ASSETS: &[(&str, &[u8])] = &[
    ("index.html", include_bytes!("../web/index.html")),
    ("style.css", include_bytes!("../web/style.css")),
    ("app.js", include_bytes!("../web/app.js")),
    ("backend.js", include_bytes!("../web/backend.js")),
    ("worker.js", include_bytes!("../web/worker.js")),
    ("editor.js", include_bytes!("../web/editor.js")),
    ("highlight.js", include_bytes!("../web/highlight.js")),
    ("gl.js", include_bytes!("../web/gl.js")),
    ("layout.js", include_bytes!("../web/layout.js")),
    ("schematic.js", include_bytes!("../web/schematic.js")),
    ("chipview.js", include_bytes!("../web/chipview.js")),
    ("panels.js", include_bytes!("../web/panels.js")),
    ("settings.js", include_bytes!("../web/settings.js")),
    ("util.js", include_bytes!("../web/util.js")),
    ("progress.js", include_bytes!("../web/progress.js")),
    ("terminal.js", include_bytes!("../web/terminal.js")),
    ("simview.js", include_bytes!("../web/simview.js")),
    ("ftdi.js", include_bytes!("../web/ftdi.js")),
    ("program.js", include_bytes!("../web/program.js")),
    ("favicon.svg", include_bytes!("../web/favicon.svg")),
];

/// Serves on `addr` (for example `127.0.0.1:8080`) until the process ends.
pub fn serve(addr: &str) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    eprintln!("synth playground: http://{}/", listener.local_addr()?);
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        std::thread::spawn(move || {
            if let Err(e) = connection(stream) {
                eprintln!("playground: {e}");
            }
        });
    }
    Ok(())
}

fn connection(stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; len.min(64 << 20)];
    reader.read_exact(&mut body)?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = path.trim_start_matches('/');
    let mut out = stream;
    match (method, path) {
        // With `?progress`, the response streams the flow's progress, one
        // JSON object per line, then the result on the last line.
        ("POST", p) if p.starts_with("api/") && query == "progress" => {
            write!(
                out,
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"
            )?;
            out.flush()?;
            let mut lines = out.try_clone()?;
            let resp = crate::handle_observed(&p[4..], &String::from_utf8_lossy(&body), move |j| {
                // A client that went away no longer reads: the run goes on.
                let _ = writeln!(lines, "{}", j.dump()).and_then(|()| lines.flush());
            });
            out.write_all(resp.as_bytes())?;
            out.flush()
        }
        ("POST", p) if p.starts_with("api/") => {
            let resp = crate::handle(&p[4..], &String::from_utf8_lossy(&body));
            respond(&mut out, "200 OK", "application/json", resp.as_bytes())
        }
        ("GET" | "HEAD", p) => {
            let p = if p.is_empty() { "index.html" } else { p };
            // While developing the app, serve its files from disk.
            if let Ok(dir) = std::env::var("SYNTH_PLAYGROUND_DIR")
                && !p.contains("..")
                && let Ok(data) = std::fs::read(std::path::Path::new(&dir).join(p))
            {
                return respond(&mut out, "200 OK", mime(p), &data);
            }
            match ASSETS.iter().find(|(n, _)| *n == p) {
                Some((_, data)) => respond(&mut out, "200 OK", mime(p), data),
                None => respond(&mut out, "404 Not Found", "text/plain", b"not found"),
            }
        }
        _ => respond(&mut out, "405 Method Not Allowed", "text/plain", b"method not allowed"),
    }
}

fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "svg" => "image/svg+xml",
        "wasm" => "application/wasm",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

fn respond(out: &mut TcpStream, status: &str, mime: &str, body: &[u8]) -> std::io::Result<()> {
    write!(
        out,
        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    out.write_all(body)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_web_file_is_embedded() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web");
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            if e.path().is_file() {
                let name = e.file_name().into_string().unwrap();
                assert!(super::ASSETS.iter().any(|(n, _)| *n == name), "{name} is not in ASSETS");
            }
        }
    }
}
