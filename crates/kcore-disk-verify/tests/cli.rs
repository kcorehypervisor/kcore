//! End-to-end runs of the binary against a regular file.

use std::process::Command;

const MIB: u64 = 1024 * 1024;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_kcore-disk-verify"))
}

fn image(dir: &tempfile::TempDir, mib: u64) -> String {
    let path = dir.path().join("disk.img");
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(mib * MIB).unwrap();
    path.to_string_lossy().to_string()
}

fn run(phase: &str, dev: &str, nonce: &str, extra: &[&str]) -> (i32, serde_json::Value, String) {
    let out = bin()
        .args([phase, "--device", dev, "--nonce", nonce, "--serial", "SER1"])
        .args(["--allow-file", "--no-direct", "--regions", "8"])
        .args(extra)
        .output()
        .unwrap();
    let json = serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null);
    (
        out.status.code().unwrap(),
        json,
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn write_then_verify_passes_and_emits_controller_json() {
    let dir = tempfile::tempdir().unwrap();
    let dev = image(&dir, 64);

    let (code, w, err) = run("write", &dev, "run1", &["--yes", "--machine-id", "m-1"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(w["phase"], "write");
    assert_eq!(w["passed"], true);
    assert_eq!(w["machine_id"], "m-1");
    assert_eq!(w["drive_serial"], "SER1");

    let (code, v, _) = run("verify", &dev, "run1", &[]);
    assert_eq!(code, 0);
    assert_eq!(v["phase"], "verify");
    assert_eq!(v["regions_total"], 8);
}

#[test]
fn a_disk_that_lost_its_data_exits_2_with_zeroed_regions() {
    let dir = tempfile::tempdir().unwrap();
    let dev = image(&dir, 64);
    assert_eq!(run("write", &dev, "run1", &["--yes"]).0, 0);

    // Simulate the power cycle wiping the disk.
    std::fs::write(&dev, vec![0u8; (64 * MIB) as usize]).unwrap();

    let (code, v, _) = run("verify", &dev, "run1", &[]);
    assert_eq!(code, 2);
    assert_eq!(v["passed"], false);
    assert_eq!(v["regions_zeroed"], 8);
}

#[test]
fn write_requires_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let dev = image(&dir, 64);
    let (code, _, err) = run("write", &dev, "run1", &[]);
    assert_eq!(code, 1);
    assert!(err.contains("--yes"), "{err}");
    // Nothing was written.
    assert!(std::fs::read(&dev).unwrap().iter().all(|&b| b == 0));
}

#[test]
fn files_are_refused_without_allow_file() {
    let dir = tempfile::tempdir().unwrap();
    let dev = image(&dir, 64);
    let out = bin()
        .args(["verify", "--device", &dev, "--nonce", "r", "--serial", "S"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a block device"));
}

#[test]
fn bad_arguments_are_rejected() {
    for args in [
        vec!["erase", "--device", "x", "--nonce", "n"],
        vec!["verify", "--nonce", "n"],
        vec!["verify", "--device", "x", "--nonce", "bad nonce"],
        vec!["verify", "--device", "x", "--nonce", "n", "--regions", "0"],
        vec!["verify", "--device", "x", "--nonce", "n", "--bogus"],
    ] {
        let out = bin().args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{args:?}");
    }
}
