//! The carry policy must reach lowering, and reject invalid/overflowing widths.
use std::process::Command;

#[test]
fn carry_threshold_changes_arithmetic_mapping() {
    let dir = std::env::temp_dir().join(format!("synth-carry-cli-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(dir.clone());
    std::fs::write(
        dir.join("top.v"),
        "module top(input [4:0] a, b, output [4:0] y); assign y = a + b; endmodule",
    )
    .unwrap();
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_synth"))
            .current_dir(&dir)
            .args(["synth", "top.v", "--top", "top", "--verilog", "mapped.v"])
            .args(extra)
            .output()
            .unwrap()
    };
    let baseline = run(&[]);
    assert!(baseline.status.success());
    assert!(std::fs::read_to_string(dir.join("mapped.v")).unwrap().contains("SB_CARRY"));
    let gates = run(&["--carry-min-width", "8"]);
    assert!(gates.status.success(), "{}", String::from_utf8_lossy(&gates.stderr));
    assert!(!std::fs::read_to_string(dir.join("mapped.v")).unwrap().contains("SB_CARRY"));
    for invalid in ["0", "abc", &usize::MAX.to_string()] {
        let output = run(&["--carry-min-width", invalid]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("--carry-min-width must be"));
    }
}

#[test]
fn automatic_carry_choice_matches_the_shallower_bcd_candidate() {
    let dir = std::env::temp_dir().join(format!("synth-auto-carry-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/designs/euler/bcd.v");
    let run = |extra: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_synth"))
            .current_dir(&dir)
            .args([
                "synth",
                source.to_str().unwrap(),
                "--top",
                "euler_bcd_add",
                "-P",
                "N=4",
                "--verilog",
                "mapped.v",
                "--rename",
                "top",
            ])
            .args(extra)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        (
            std::fs::read_to_string(dir.join("mapped.v")).unwrap(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let (automatic, log) = run(&[]);
    assert!(log.contains("kept threshold 8"));
    let (eight, _) = run(&["--carry-min-width", "8"]);
    let (three, fixed_log) = run(&["--carry-min-width", "3"]);
    assert_eq!(automatic, eight);
    assert_ne!(automatic, three);
    assert!(!fixed_log.contains("carry exploration"));
    std::fs::remove_dir_all(dir).unwrap();
}
