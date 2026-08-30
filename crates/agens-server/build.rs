//! Generates the coordinator's wire types from `proto/`.
//!
//! The descriptor set is produced by `protox`, a protobuf compiler that is a
//! Rust dependency, rather than by shelling out to `protoc`. The gate builds
//! inside the repository's dev shell, and a build script that needs a binary
//! nobody declared fails there for a reason no error message explains.
//!
//! The generated code lands in `OUT_DIR` and is never committed: a checked-in
//! copy is a second definition of the wire that can disagree with the `.proto`
//! next to it.
//!
//! The same script also stamps the binary with the commit it was built from,
//! because the attach handshake compares a client's build against a daemon
//! that may have been compiled days earlier. Asking git here is what a
//! manually bumped constant cannot be: it changes on every commit without
//! anybody remembering to change it.

use std::io::Result;
use std::path::Path;
use std::process::Command;

const PROTO: &str = "proto/agens/coordinator/v1/coordinator.proto";
const INCLUDE: &str = "proto";

/// The commit this build is from, or `unknown` for sources outside a git
/// checkout (a source tarball, a store path). `unknown` on both sides of the
/// handshake compares equal on purpose: two such builds are indistinguishable,
/// and refusing them would refuse every daemon such a distribution starts.
fn git_commit() -> String {
    let described = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output();

    match described {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }
        _ => "unknown".to_owned(),
    }
}

/// Points cargo at the file git moves on every commit, so a rebuild after a
/// commit re-stamps the binary instead of keeping the previous hash.
fn track_git_head() {
    let head = Command::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .output();

    if let Ok(output) = head
        && output.status.success()
    {
        let git_dir = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        println!("cargo:rerun-if-changed={git_dir}/refs/heads");
    }
}

fn main() -> Result<()> {
    println!("cargo:rerun-if-changed={PROTO}");
    println!("cargo:rerun-if-changed={INCLUDE}");
    println!("cargo:rustc-env=AGENS_GIT_COMMIT={}", git_commit());
    track_git_head();

    let descriptors = protox::compile([Path::new(PROTO)], [Path::new(INCLUDE)])
        .map_err(|error| std::io::Error::other(error.to_string()))?;

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_fds(descriptors)
}
