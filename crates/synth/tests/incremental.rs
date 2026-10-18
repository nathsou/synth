//! CLI persistence and invalidation, including replacement of corrupt files.
use std::process::Command;

#[test]
fn checkpoint_lifecycle() {
    let dir = std::env::temp_dir().join(format!("synth-incremental-cli-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(dir.clone());
    let rtl = "module top(input clk, output led); reg [7:0] count=0;
        always @(posedge clk) count <= count + 1; assign led=count[7]; endmodule";
    std::fs::write(dir.join("top.v"), rtl).unwrap();
    let run = |extra: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_synth"))
            .current_dir(&dir)
            .args([
                "build",
                "top.v",
                "--device",
                "hx1k",
                "--package",
                "vq100",
                "--incremental",
                "design.chk",
                "-o",
                "design.bin",
            ])
            .args(extra)
            .output()
            .unwrap();
        let log = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{log}");
        log
    };
    run(&[]);
    let first = std::fs::read(dir.join("design.bin")).unwrap();
    let checkpoint = std::fs::read(dir.join("design.chk")).unwrap();
    assert!(run(&[]).contains("incremental routing: reused"));
    assert_eq!(first, std::fs::read(dir.join("design.bin")).unwrap());
    assert_eq!(checkpoint, std::fs::read(dir.join("design.chk")).unwrap());
    std::fs::write(dir.join("design.chk"), b"truncated").unwrap();
    assert!(run(&[]).contains("ignoring checkpoint"));
    assert_eq!(checkpoint, std::fs::read(dir.join("design.chk")).unwrap());
    assert!(run(&["--seed", "2"]).contains("incompatible"));
    let checkpoint = std::fs::read(dir.join("design.chk")).unwrap();
    // An allowed timing failure still must not replace the reusable checkpoint.
    run(&["--freq", "100000", "--timing-allow-fail"]);
    assert_eq!(checkpoint, std::fs::read(dir.join("design.chk")).unwrap());
    // Bare --incremental automatically selects a design-specific checkpoint.
    let auto = run(&["--incremental"]);
    assert!(auto.contains("incremental checkpoint: .synth/checkpoints/"));
    assert!(!auto.contains("incremental placement:"));
    let checkpoint_paths = || {
        let mut paths: Vec<_> = std::fs::read_dir(dir.join(".synth/checkpoints"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        paths.sort();
        paths
    };
    let selected = checkpoint_paths();
    assert_eq!(selected.len(), 1);
    // Output names and source edits do not change the selected checkpoint.
    std::fs::write(dir.join("top.v"), format!("// edited\n{rtl}")).unwrap();
    assert!(run(&["--incremental", "-o", "other.bin"]).contains("incremental routing: reused"));
    assert_eq!(selected, checkpoint_paths());
    run(&["--incremental", "--seed", "3"]);
    assert_eq!(checkpoint_paths().len(), 2);
    assert!(run(&["--incremental=", "--seed", "3"]).contains("incremental routing: reused"));
}
