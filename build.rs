//! Embeds the gateway's version as `GATEWAY_VERSION`: the git tag it was built from, as
//! `git describe --tags` names it (`v1.0.0`, or `v1.0.0-16-ge9477e1` between tags). The Docker
//! build has no `.git`, so the release workflow passes the tag in as the `GATEWAY_VERSION`
//! build argument, which wins when set. Without either the version is `dev`.
//!
//! Also writes `aws_regions.json` to `OUT_DIR`: the AWS regions the official SDK knows, with their
//! names, for the console's Bedrock region list. The SDK keeps that list (its endpoint partitions)
//! inside each service crate, not public, so it is read from `aws-sdk-bedrockruntime`'s source as
//! Cargo downloaded it. Without the source (an unusual setup) the list is empty and the console
//! asks for the region as text, unless `GATEWAY_REQUIRE_AWS_REGIONS` is set (the Docker build sets
//! it): then the build fails.

use std::{path::PathBuf, process::Command};

/// The crate whose built-in partitions are read, and the partitions the console offers. The ISO
/// partitions (US and UK classified regions) are left out.
const SDK_CRATE: &str = "aws-sdk-bedrockruntime";
const PARTITIONS: &[&str] = &["aws", "aws-eusc", "aws-us-gov", "aws-cn"];

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

    // A new SDK version brings new regions.
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-env-changed=GATEWAY_REQUIRE_AWS_REGIONS");
    let regions = match aws_regions() {
        Ok(r) => r,
        Err(why) if std::env::var_os("GATEWAY_REQUIRE_AWS_REGIONS").is_some() => panic!("no AWS region list for the console: {why}"),
        Err(why) => {
            println!("cargo:warning=no AWS region list for the console ({why}); Bedrock regions are entered as text");
            serde_json::Value::Array(Vec::new())
        }
    };
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("aws_regions.json");
    std::fs::write(out, regions.to_string()).expect("write aws_regions.json");
}

fn git_describe() -> Option<String> {
    let out = Command::new("git").args(["describe", "--tags", "--always"]).output().ok()?;
    let v = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !v.is_empty()).then_some(v)
}

/// `[{ "id": "eu-central-1", "name": "Europe (Frankfurt)", "partition": "aws" }, …]`, from the SDK
/// crate's endpoint partitions.
fn aws_regions() -> Result<serde_json::Value, String> {
    let lock = std::fs::read_to_string("Cargo.lock").map_err(|e| format!("Cargo.lock: {e}"))?;
    let version = lock
        .split("[[package]]")
        .find(|p| p.contains(&format!("name = \"{SDK_CRATE}\"")))
        .and_then(|p| p.lines().find_map(|l| l.strip_prefix("version = \"")?.strip_suffix('"')))
        .ok_or_else(|| format!("{SDK_CRATE} is not in Cargo.lock"))?;
    let source = crate_source(&format!("{SDK_CRATE}-{version}"))?;
    println!("cargo:rerun-if-changed={}", source.display());
    let text = std::fs::read_to_string(&source).map_err(|e| format!("{}: {e}", source.display()))?;
    // `PartitionResolver::new_from_json(b"{…}")`: a Rust byte string with `\"` and `\\` escapes.
    let literal = text
        .split_once("new_from_json(b\"")
        .and_then(|(_, rest)| rest.split_once("\")"))
        .map(|(json, _)| json)
        .ok_or("the SDK's partition data has moved")?;
    let json = literal.replace("\\\"", "\"").replace("\\\\", "\\");
    let data: serde_json::Value = serde_json::from_str(&json).map_err(|e| format!("partition data: {e}"))?;
    let mut regions = Vec::new();
    for p in data["partitions"].as_array().into_iter().flatten() {
        let id = p["id"].as_str().unwrap_or_default();
        if !PARTITIONS.contains(&id) {
            continue;
        }
        for (region, info) in p["regions"].as_object().into_iter().flatten() {
            // `aws-global` and the like name a partition's global endpoint, not a region.
            if region.ends_with("-global") {
                continue;
            }
            regions.push(serde_json::json!({ "id": region, "name": info["description"].as_str().unwrap_or(region), "partition": id }));
        }
    }
    if regions.is_empty() {
        return Err("the SDK's partition data lists no regions".into());
    }
    Ok(serde_json::Value::Array(regions))
}

/// `src/endpoint_lib.rs` of a downloaded crate: in a vendor directory, or in Cargo's registry.
fn crate_source(dir: &str) -> Result<PathBuf, String> {
    let file = |root: PathBuf| root.join(dir).join("src").join("endpoint_lib.rs");
    let vendored = file(PathBuf::from("vendor"));
    if vendored.exists() {
        return Ok(vendored);
    }
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(|h| PathBuf::from(h).join(".cargo")))
        .ok_or("no CARGO_HOME")?;
    let registry = home.join("registry").join("src");
    let indexes = std::fs::read_dir(&registry).map_err(|e| format!("{}: {e}", registry.display()))?;
    indexes
        .filter_map(Result::ok)
        .map(|index| file(index.path()))
        .find(|f| f.exists())
        .ok_or_else(|| format!("{dir} is not in {}", registry.display()))
}
