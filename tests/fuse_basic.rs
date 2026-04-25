//! Integration test for the v0.2 FUSE mount.
//!
//! Runs the actual `claude-cellar` binary, mounts a FUSE on a temp dir,
//! exercises basic read/write/multi-writer scenarios, then unmounts.
//!
//! Marked `#[ignore]` because FUSE may not be available in every CI
//! environment. Run with `cargo test --release -- --ignored` on Linux
//! with `fusermount3` installed.

#![cfg(target_os = "linux")]

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

fn cellar_bin() -> String {
    env!("CARGO_BIN_EXE_claude-cellar").to_string()
}

fn umount(p: &std::path::Path) {
    let _ = Command::new("fusermount3").arg("-u").arg(p).status();
}

#[test]
#[ignore]
fn end_to_end_read_write_multi() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let mount = tmp.path().join("mount");
    fs::create_dir_all(store.join("proj-A")).unwrap();
    fs::create_dir_all(&mount).unwrap();

    // Seed a compressed session.
    let seed = tmp.path().join("seed.jsonl");
    fs::write(&seed, b"alpha\nbeta\n").unwrap();
    let st = Command::new(cellar_bin())
        .args(["compress"])
        .arg(&seed)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(st.success());
    fs::rename(
        seed.with_extension("jsonl.zst"),
        store.join("proj-A").join("abc.jsonl.zst"),
    )
    .unwrap();

    // Mount in foreground in a child process (we'll umount to terminate).
    let mut child = Command::new(cellar_bin())
        .args(["mount", "--foreground", "--store-dir"])
        .arg(&store)
        .arg("--mount-dir")
        .arg(&mount)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    thread::sleep(Duration::from_millis(800));

    // Read-only roundtrip
    let got = fs::read_to_string(mount.join("proj-A").join("abc.jsonl")).unwrap();
    assert_eq!(got, "alpha\nbeta\n");

    // Append
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(mount.join("proj-A").join("abc.jsonl"))
        .unwrap();
    writeln!(f, "gamma").unwrap();
    drop(f);
    thread::sleep(Duration::from_millis(200));
    let got = fs::read_to_string(mount.join("proj-A").join("abc.jsonl")).unwrap();
    assert_eq!(got, "alpha\nbeta\ngamma\n");

    // Create new session
    fs::write(mount.join("proj-A").join("new.jsonl"), b"x\ny\n").unwrap();
    thread::sleep(Duration::from_millis(200));
    let got = fs::read_to_string(mount.join("proj-A").join("new.jsonl")).unwrap();
    assert_eq!(got, "x\ny\n");

    // mkdir + create
    fs::create_dir(mount.join("proj-B")).unwrap();
    fs::write(mount.join("proj-B").join("z.jsonl"), b"in-b\n").unwrap();
    thread::sleep(Duration::from_millis(200));

    // Multi-writer: 4 threads, each writes 200 lines to its own file.
    fs::create_dir(mount.join("proj-multi")).unwrap();
    let handles: Vec<_> = (1..=4u32)
        .map(|i| {
            let path = mount.join("proj-multi").join(format!("sess-{i}.jsonl"));
            thread::spawn(move || {
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(&path)
                    .unwrap();
                for j in 1..=200u32 {
                    writeln!(f, "writer-{i} line-{j}").unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    thread::sleep(Duration::from_millis(300));
    for i in 1..=4u32 {
        let p = mount.join("proj-multi").join(format!("sess-{i}.jsonl"));
        let lines = fs::read_to_string(&p).unwrap().lines().count();
        assert_eq!(lines, 200, "sess-{i} expected 200 lines, got {lines}");
    }

    // Teardown.
    umount(&mount);
    let _ = child.wait();

    // Verify store contents persist and decompress.
    for proj in ["proj-A", "proj-B", "proj-multi"] {
        let proj_path = store.join(proj);
        for entry in fs::read_dir(&proj_path).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if s.ends_with(".jsonl.zst") {
                let out = tmp.path().join(format!("v-{s}"));
                let st = Command::new(cellar_bin())
                    .args(["decompress"])
                    .arg(entry.path())
                    .arg(&out)
                    .stdout(Stdio::null())
                    .status()
                    .unwrap();
                assert!(st.success(), "decompress {s} failed");
                let meta = fs::metadata(&out).unwrap();
                assert!(meta.len() > 0, "{s} decompressed empty");
            }
        }
    }
}
