//! Grava o SHA do commit do build em `RRQ_BUILD_SHA`: `GITHUB_SHA` no CI, `git rev-parse HEAD`
//! fora dele, `unknown` sem git. O `rrq update` compara com o `version.txt` da release `main-build`.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    let git = Path::new(".git");
    if git.is_dir() {
        println!("cargo:rerun-if-changed=.git/HEAD");
        println!("cargo:rerun-if-changed=.git/packed-refs");
        if let Ok(head) = std::fs::read_to_string(git.join("HEAD")) {
            if let Some(r) = head.trim().strip_prefix("ref: ") {
                println!("cargo:rerun-if-changed=.git/{r}");
            }
        }
    }
    let sha = std::env::var("GITHUB_SHA")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            let out = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=RRQ_BUILD_SHA={sha}");
}
