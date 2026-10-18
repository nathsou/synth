//! JSON-RPC messages with `Content-Length` framing (the LSP base protocol).

use std::io::{self, BufRead, Write};

use synth_core::Json;

/// Reads one message; `None` at end of input.
pub fn read(input: &mut impl BufRead) -> io::Result<Option<Json>> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            if len.is_some() {
                break;
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; len.unwrap_or(0)];
    input.read_exact(&mut body)?;
    let text = String::from_utf8_lossy(&body);
    Json::parse(&text).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn write(out: &mut impl Write, msg: &Json) -> io::Result<()> {
    let body = msg.dump();
    write!(out, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    out.flush()
}

pub fn response(id: &Json, result: Json) -> Json {
    Json::obj().with("jsonrpc", "2.0").with("id", id.clone()).with("result", result)
}

pub fn error(id: &Json, code: i64, message: &str) -> Json {
    Json::obj()
        .with("jsonrpc", "2.0")
        .with("id", id.clone())
        .with("error", Json::obj().with("code", code).with("message", message))
}

pub fn notification(method: &str, params: Json) -> Json {
    Json::obj().with("jsonrpc", "2.0").with("method", method).with("params", params)
}
