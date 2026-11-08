//! Boards and projects: `synth init`, `synth prog`, `synth build --board`
//! and `synth.toml`.
//!
//! A project is a directory with a `synth.toml`:
//!
//! ```toml
//! board = "alchitry-cu"
//! top = "top"
//! sources = ["top.weft"]
//! constraints = "pins.pcf"
//! output = "build/top.bin"
//! options = ["--freq", "50"]
//! ```
//!
//! `synth build` with no source files builds it; `synth prog` programs its
//! bitstream onto the board.

use std::path::Path;

use synth_playground::{BOARDS, Board, Family};

use crate::{read, write};

pub const PROJECT_FILE: &str = "synth.toml";

/// The board named `id` on the command line.
pub fn board(id: &str) -> Result<&'static Board, String> {
    BOARDS
        .iter()
        .find(|b| b.id == id)
        .ok_or_else(|| format!("unknown board `{id}`; the boards are:\n{}", board_list()))
}

/// The boards, one per line, for help and errors.
pub fn board_list() -> String {
    BOARDS
        .iter()
        .map(|b| {
            let device = match Family::of(b.part) {
                Family::Ice40 => {
                    format!("iCE40 {} {}", b.part.to_uppercase(), b.package.to_uppercase())
                }
                Family::Artix7 => format!("Artix-7 {}", device(b).to_uppercase()),
            };
            format!("    {:<14} {} ({device}, {} MHz)\n", b.id, b.name, b.clock_hz / 1_000_000)
        })
        .collect()
}

/// The `--device` of a board: the iCE40 part, or the Artix-7 part with its
/// package and speed grade.
fn device(b: &Board) -> String {
    match Family::of(b.part) {
        Family::Ice40 => b.part.to_string(),
        Family::Artix7 => format!("{}{}-1", b.part, b.package),
    }
}

/// The options that select a board's device.
fn device_args(b: &Board) -> Vec<String> {
    match Family::of(b.part) {
        Family::Ice40 => ["--device", b.part, "--package", b.package].map(String::from).into(),
        Family::Artix7 => vec!["--family".into(), "artix7".into(), "--device".into(), device(b)],
    }
}

/// The option that gives a board's constraints file.
fn constraints_option(b: &Board) -> &'static str {
    match Family::of(b.part) {
        Family::Ice40 => "--pcf",
        Family::Artix7 => "--xdc",
    }
}

fn bitstream_ext(b: &Board) -> &'static str {
    match Family::of(b.part) {
        Family::Ice40 => "bin",
        Family::Artix7 => "bit",
    }
}

/// Whether the arguments name source files.
fn has_sources(argv: &[String]) -> bool {
    argv.iter()
        .any(|a| !a.starts_with('-') && [".v", ".sv", ".weft"].iter().any(|e| a.ends_with(e)))
}

/// Removes `<name> <value>` (or `<name>=<value>`) from the arguments.
fn take_option(argv: &mut Vec<String>, name: &str) -> Result<Option<String>, String> {
    let mut found = None;
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == name {
            let v =
                argv.get(i + 1).cloned().ok_or_else(|| format!("option {name} needs a value"))?;
            argv.drain(i..i + 2);
            found = Some(v);
        } else if let Some(v) = argv[i].strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
            found = Some(v.to_string());
            argv.remove(i);
        } else {
            i += 1;
        }
    }
    Ok(found)
}

/// The arguments of `synth build`, with a board's or a project's settings
/// put first: options given on the command line come later and win.
///
/// - With `--board <id>`: the board's device, and its pin constraints
///   unless a constraints file is given.
/// - Without source files, in a directory with a `synth.toml`: the
///   project's sources, top, board, constraints, output and options.
pub fn build_args(argv: &[String]) -> Result<Vec<String>, String> {
    let mut argv = argv.to_vec();
    let board_id = take_option(&mut argv, "--board")?;
    if board_id.is_none() && !has_sources(&argv) && Path::new(PROJECT_FILE).exists() {
        let p = Project::load(PROJECT_FILE)?;
        let mut args = p.build_args()?;
        if let Some(dir) = Path::new(&p.output).parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        args.extend(argv);
        return Ok(args);
    }
    let Some(id) = board_id else { return Ok(argv) };
    let b = board(&id)?;
    let mut args = device_args(b);
    let given = ["--pcf", "--xdc"]
        .iter()
        .any(|o| argv.iter().any(|a| a == o || a.starts_with(&format!("{o}="))));
    if !given {
        // The board's own pins, written where the checkpoints go.
        let dir = ".synth/boards";
        std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
        let path = format!("{dir}/{}.{}", b.id, &constraints_option(b)[2..]);
        write(&path, b.pins.as_bytes())?;
        args.extend([constraints_option(b).to_string(), path]);
    }
    args.extend(argv);
    Ok(args)
}

/// A `synth.toml`.
#[derive(Debug, Default)]
pub struct Project {
    pub board: String,
    pub top: Option<String>,
    pub sources: Vec<String>,
    pub constraints: Option<String>,
    pub output: String,
    /// The Artix-7 database, for the Artix-7 boards.
    pub db: Option<String>,
    pub options: Vec<String>,
}

#[derive(Debug, PartialEq)]
enum Value {
    Str(String),
    List(Vec<String>),
}

impl Project {
    pub fn load(path: &str) -> Result<Project, String> {
        Project::parse(&read(path)?).map_err(|e| format!("{path}: {e}"))
    }

    /// Reads the subset of TOML a project uses: `key = "string"` and
    /// `key = ["string", ...]` on one line, and `#` comments.
    pub fn parse(src: &str) -> Result<Project, String> {
        let mut p = Project::default();
        let mut output = None;
        let mut board_id = None;
        for (no, line) in src.lines().enumerate() {
            let err = |m: &str| format!("line {}: {m}", no + 1);
            let line = strip_comment(line).trim();
            if line.is_empty() {
                continue;
            }
            let (key, value) = line.split_once('=').ok_or_else(|| err("expected `key = value`"))?;
            let value = parse_value(value.trim()).ok_or_else(|| {
                err("expected a \"string\" or a list of strings [\"a\", \"b\"] on one line")
            })?;
            let key = key.trim();
            match (key, value) {
                ("board", Value::Str(s)) => board_id = Some(s),
                ("top", Value::Str(s)) => p.top = Some(s),
                ("constraints", Value::Str(s)) => p.constraints = Some(s),
                ("output", Value::Str(s)) => output = Some(s),
                ("db", Value::Str(s)) => p.db = Some(s),
                ("sources", Value::List(v)) => p.sources = v,
                ("options", Value::List(v)) => p.options = v,
                ("sources" | "options", Value::Str(_)) => {
                    return Err(err(&format!("`{key}` is a list: {key} = [\"...\"]")));
                }
                ("board" | "top" | "constraints" | "output" | "db", Value::List(_)) => {
                    return Err(err(&format!("`{key}` is a string")));
                }
                _ => {
                    return Err(err(&format!(
                        "unknown key `{key}` (board, top, sources, constraints, output, db, options)"
                    )));
                }
            }
        }
        p.board = board_id.ok_or("no `board`")?;
        let b = board(&p.board)?;
        if p.sources.is_empty() {
            return Err("no `sources`".into());
        }
        p.output = output.unwrap_or_else(|| format!("build/top.{}", bitstream_ext(b)));
        Ok(p)
    }

    fn build_args(&self) -> Result<Vec<String>, String> {
        let b = board(&self.board)?;
        let mut args = self.sources.clone();
        if let Some(t) = &self.top {
            args.extend(["--top".into(), t.clone()]);
        }
        args.extend(device_args(b));
        if let Some(c) = &self.constraints {
            args.extend([constraints_option(b).into(), c.clone()]);
        }
        if let Some(db) = &self.db {
            args.extend(["--db".into(), db.clone()]);
        }
        args.extend(["-o".into(), self.output.clone()]);
        args.extend(self.options.iter().cloned());
        Ok(args)
    }
}

/// The line without a `#` comment outside strings.
fn strip_comment(line: &str) -> &str {
    let mut in_str = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '#' if !in_str => return &line[..i],
            _ => {}
        }
    }
    line
}

fn parse_string(s: &str) -> Option<(String, &str)> {
    let rest = s.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((out, &rest[i + 1..])),
            '\\' => match chars.next()?.1 {
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                'n' => out.push('\n'),
                't' => out.push('\t'),
                _ => return None,
            },
            c => out.push(c),
        }
    }
    None
}

fn parse_value(s: &str) -> Option<Value> {
    if let Some(mut rest) = s.strip_prefix('[') {
        let mut items = Vec::new();
        loop {
            rest = rest.trim_start();
            if let Some(after) = rest.strip_prefix(']') {
                return after.trim().is_empty().then_some(Value::List(items));
            }
            let (item, after) = parse_string(rest)?;
            items.push(item);
            rest = after.trim_start();
            rest = rest.strip_prefix(',').unwrap_or(rest);
        }
    }
    let (v, rest) = parse_string(s)?;
    rest.trim().is_empty().then_some(Value::Str(v))
}

/// A string as a TOML basic string.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The starter design: the LEDs count in binary, a few times a second.
fn starter(b: &Board) -> String {
    format!(
        "\
// Counts in binary on four LEDs, a few times a second.
// `synth build` builds it and `synth prog` programs the {name}.

/// CLK_HZ: the clock frequency ({mhz} MHz on the {name}); W: the counter's
/// width, so that its top four bits count a few times a second.
module top<CLK_HZ: int = {hz}, W: int = bits_for(CLK_HZ) + 1>(
    in  clk: clock,
    out led: uint<4>,
) {{
    reg count: uint<W> = 0;
    count := count +% 1;
    led = count[W - 1:W - 4];
}}
",
        name = b.name,
        mhz = b.clock_hz / 1_000_000,
        hz = group_digits(b.clock_hz),
    )
}

/// `100000000` as `100_000_000`.
fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('_');
        }
        out.push(c);
    }
    out
}

fn project_file(b: &Board, constraints: &str) -> String {
    let mut s = format!(
        "\
# A synth project: `synth build` builds it, `synth prog` programs the board.
# Options given on the command line override these.
board = {board}
top = \"top\"
sources = [\"top.weft\"]
constraints = {constraints}
output = {output}
# More options for `synth build`, for example [\"--freq\", \"50\", \"--tries\", \"1\"].
options = []
",
        board = quote(b.id),
        constraints = quote(constraints),
        output = quote(&format!("build/top.{}", bitstream_ext(b))),
    );
    if Family::of(b.part) == Family::Artix7 {
        s.push_str(
            "# The Artix-7 database (tools/fetch_xc7_db.sh fetches it), unless\n\
             # SYNTH_XC7_DB gives it:\n\
             # db = \"/path/to/prjxray-db/artix7\"\n",
        );
    }
    s
}

/// `synth init --board <id> [<dir>] [--force]`, `synth init --list`.
pub fn cmd_init(argv: &[String]) -> Result<(), String> {
    let mut a = crate::args::Args::parse(argv, &["--board"], &["--force", "--list"])?;
    if a.flag("--list") {
        print!("{}", board_list());
        return Ok(());
    }
    let id = a
        .value("--board")
        .ok_or_else(|| format!("missing --board <id>; the boards are:\n{}", board_list()))?;
    let b = board(&id)?;
    let dir = a.positional(0, "directory").unwrap_or_else(|_| ".".into());
    std::fs::create_dir_all(&dir).map_err(|e| format!("{dir}: {e}"))?;
    let constraints = format!("pins.{}", &constraints_option(b)[2..]);
    let files = [
        (PROJECT_FILE.to_string(), project_file(b, &constraints)),
        (constraints.clone(), b.pins.to_string()),
        ("top.weft".to_string(), starter(b)),
    ];
    let path = |f: &str| Path::new(&dir).join(f).to_string_lossy().into_owned();
    if !a.flag("--force") {
        let existing: Vec<String> =
            files.iter().map(|(f, _)| path(f)).filter(|p| Path::new(p).exists()).collect();
        if !existing.is_empty() {
            return Err(format!("{} already exist(s) (--force overwrites)", existing.join(", ")));
        }
    }
    for (f, content) in &files {
        write(&path(f), content.as_bytes())?;
    }
    let ignore = path(".gitignore");
    if !Path::new(&ignore).exists() {
        write(&ignore, b"/build/\n/.synth/\n")?;
    }
    info!("created a project for the {} in {dir}", b.name);
    let cd = if dir == "." { String::new() } else { format!("cd {dir} && ") };
    let db = if Family::of(b.part) == Family::Artix7 {
        " (with SYNTH_XC7_DB or `db` in synth.toml set to the Artix-7 database)"
    } else {
        ""
    };
    info!("next: {cd}synth build{db} && synth prog");
    Ok(())
}

/// `synth prog [<bitstream>] [--board <id>] [--flash] [--cable <name>]
/// [--dry-run]`: programs a bitstream with openFPGALoader.
pub fn cmd_prog(argv: &[String]) -> Result<(), String> {
    let mut a = crate::args::Args::parse(argv, &["--board", "--cable"], &["--flash", "--dry-run"])?;
    let project =
        if Path::new(PROJECT_FILE).exists() { Some(Project::load(PROJECT_FILE)?) } else { None };
    let id = a.value("--board").or_else(|| project.as_ref().map(|p| p.board.clone())).ok_or_else(
        || {
            format!(
                "missing --board <id> (no {PROJECT_FILE} here); the boards are:\n{}",
                board_list()
            )
        },
    )?;
    let b = board(&id)?;
    let bitstream = a
        .positional(0, "bitstream")
        .ok()
        .or_else(|| project.as_ref().map(|p| p.output.clone()))
        .ok_or("missing the bitstream to program")?;
    if !a.flag("--dry-run") && !Path::new(&bitstream).exists() {
        return Err(format!("{bitstream}: no such file (run `synth build` first)"));
    }
    let mut cmd = vec!["openFPGALoader".to_string(), "-b".into(), b.loader.into()];
    if let Some(c) = a.value("--cable") {
        cmd.extend(["-c".into(), c]);
    }
    // iCE40 boards load from their flash; the others into the FPGA, unless
    // asked to write the flash (kept across power cycles).
    if b.flash_only || a.flag("--flash") {
        cmd.push("-f".into());
    }
    cmd.push(bitstream);
    if a.flag("--dry-run") {
        println!("{}", cmd.join(" "));
        return Ok(());
    }
    info!("programming the {}: {}", b.name, cmd.join(" "));
    let status = std::process::Command::new(&cmd[0]).args(&cmd[1..]).status().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "openFPGALoader not found: install it (macOS: `brew install openfpgaloader`; \
             Debian/Ubuntu: `apt install openfpgaloader`), or program from the playground \
             (`synth playground`) over WebUSB"
                .to_string()
        } else {
            format!("openFPGALoader: {e}")
        }
    })?;
    if !status.success() {
        return Err(format!("openFPGALoader failed ({status})"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_projects() {
        let p = Project::parse(
            "# comment\nboard = \"alchitry-cu\" # trailing\ntop = \"top\"\n\
             sources = [\"a.v\", \"b # c.weft\"]\noptions = []\n",
        )
        .unwrap();
        assert_eq!(p.board, "alchitry-cu");
        assert_eq!(p.sources, ["a.v", "b # c.weft"]);
        assert_eq!(p.output, "build/top.bin");
        assert!(p.options.is_empty());
        assert!(Project::parse("board = \"nope\"\nsources = [\"a.v\"]\n").is_err());
        assert!(Project::parse("board = \"alchitry-cu\"\nsources = \"a.v\"\n").is_err());
        assert!(Project::parse("board = \"alchitry-cu\"\nsource = [\"a.v\"]\n").is_err());
    }

    #[test]
    fn every_board_has_a_project() {
        for b in BOARDS {
            let c = format!("pins.{}", &constraints_option(b)[2..]);
            let p = Project::parse(&project_file(b, &c)).unwrap();
            assert_eq!(p.board, b.id);
            let args = p.build_args().unwrap();
            assert!(args.contains(&device(b)));
        }
    }

    #[test]
    fn board_arguments() {
        let argv: Vec<String> =
            ["top.v", "--board", "alchitry-au", "--xdc", "my.xdc"].map(String::from).into();
        let args = build_args(&argv).unwrap();
        assert_eq!(
            args[..4],
            ["--family", "artix7", "--device", "xc7a35tftg256-1"].map(String::from)
        );
        assert!(!args.iter().any(|a| a == "--board"));
        assert_eq!(group_digits(100_000_000), "100_000_000");
        assert_eq!(group_digits(12_000_000), "12_000_000");
    }
}
