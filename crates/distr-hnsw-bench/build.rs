//! Embed the source revision in the harness so every report names the code
//! that produced it. The revision is `<commit>` or `<commit>-dirty` when any
//! tracked or untracked file under the workspace's sources differs from that
//! commit; `DISTR_HNSW_REV` overrides it for builds outside a git checkout.

use std::{path::Path, process::Command};

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo");
    let root = Path::new(&manifest_dir).join("../..");
    for path in [".git/HEAD", ".git/index", "Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={}", root.join(path).display());
    }
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=../distr-hnsw-index/src");
    println!("cargo:rerun-if-changed=../distr-hnsw-index/Cargo.toml");
    println!("cargo:rerun-if-env-changed=DISTR_HNSW_REV");

    let revision = std::env::var("DISTR_HNSW_REV").ok().unwrap_or_else(|| {
        match git(&root, &["rev-parse", "--short=12", "HEAD"]) {
            Some(commit) => {
                let status = git(
                    &root,
                    &[
                        "status",
                        "--porcelain",
                        "--untracked-files=normal",
                        "--",
                        "crates",
                        "Cargo.toml",
                        "Cargo.lock",
                    ],
                );
                match status {
                    Some(changes) if changes.is_empty() => commit,
                    _ => format!("{commit}-dirty"),
                }
            }
            None => "unknown".to_owned(),
        }
    });
    println!("cargo:rustc-env=DISTR_HNSW_REV={revision}");
}
