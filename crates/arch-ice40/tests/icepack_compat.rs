//! Bit-exactness against Project IceStorm's `icepack`/`iceunpack`.
//!
//! The `.asc` inputs were produced by Yosys + nextpnr and the `.bin` files by
//! `icepack` from them (see `tests/data/README.md`).

use arch_ice40::Config;
use arch_ice40::asc::{read_asc, write_asc};

fn data(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn check(stem: &str) {
    let asc = String::from_utf8(data(&format!("{stem}.asc"))).unwrap();
    let bin = data(&format!("{stem}.bin"));

    // asc -> bin matches icepack exactly.
    let cfg = read_asc(&asc).unwrap();
    assert!(cfg.to_bin() == bin, "{stem}: bitstream differs from icepack output");

    // bin -> config -> bin is the identity, and agrees with the asc.
    let decoded = Config::from_bin(&bin).unwrap();
    assert!(decoded.to_bin() == bin, "{stem}: bin round trip");
    assert!(decoded.cram == cfg.cram && decoded.bram == cfg.bram, "{stem}: decode mismatch");

    // asc -> config -> asc reproduces the tile and RAM data. nextpnr orders
    // tiles differently from iceunpack, so compare the blocks as a set.
    let rewritten = write_asc(&cfg);
    assert!(blocks(&rewritten) == blocks(&asc), "{stem}: asc round trip");
    assert!(read_asc(&rewritten).unwrap() == cfg, "{stem}: asc reparse");
}

/// Splits an `.asc` file into its statement blocks (header line plus data
/// lines), sorted. Ignores the free-form comment, nextpnr's `.sym` names and
/// all-zero RAM contents (nextpnr omits those; iceunpack writes them).
fn blocks(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        if line.starts_with('.') {
            out.push(String::new());
        }
        if let Some(b) = out.last_mut() {
            b.push_str(line);
            b.push('\n');
        }
    }
    out.retain(|b| {
        let zero_ram = b.starts_with(".ram_data")
            && b.lines().skip(1).all(|l| l.bytes().all(|c| c == b'0'));
        !(b.starts_with(".comment") || b.starts_with(".sym") || zero_ram)
    });
    out.sort();
    out
}

#[test]
fn hx1k_blinky_ram() {
    check("hx1k_blinky_ram");
}

#[test]
fn hx8k_blinky_ram() {
    check("hx8k_blinky_ram");
}
