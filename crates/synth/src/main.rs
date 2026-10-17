//! The `synth` command-line tool.

use std::process::ExitCode;

const USAGE: &str = "\
synth — unified FPGA toolchain

USAGE:
    synth <COMMAND> [OPTIONS]

COMMANDS:
    help       Print this message
    version    Print version information

Nothing else is implemented yet; see docs/ROADMAP.md.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("help" | "-h" | "--help") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("version" | "-V" | "--version") => {
            println!("synth {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("error: unknown command `{other}`\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
