//! Embeds the gateway's version as `GATEWAY_VERSION`: the git tag it was built from, as
//! `git describe --tags` names it (`v1.0.0`, or `v1.0.0-16-ge9477e1` between tags). The Docker
//! build has no `.git`, so the release workflow passes the tag in as the `GATEWAY_VERSION`
//! build argument, which wins when set. Without either the version is `dev`.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=GATEWAY_VERSION");
    // A new commit or tag changes these, so the version is not stale after `cargo build`.
    for path in [".git/HEAD", ".git/refs", ".git/packed-refs"] {
        println!("cargo:rerun-if-changed={path}");
    }
    let version = std::env::var("GATEWAY_VERSION")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or_else(git_describe)
        .unwrap_or_else(|| "dev".into());
    println!("cargo:rustc-env=GATEWAY_VERSION={version}");
}

fn git_describe() -> Option<String> {
    let out = Command::new("git").args(["describe", "--tags", "--always"]).output().ok()?;
    let v = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !v.is_empty()).then_some(v)
}
