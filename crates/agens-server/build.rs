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

use std::io::Result;
use std::path::Path;

const PROTO: &str = "proto/agens/coordinator/v1/coordinator.proto";
const INCLUDE: &str = "proto";

fn main() -> Result<()> {
    println!("cargo:rerun-if-changed={PROTO}");
    println!("cargo:rerun-if-changed={INCLUDE}");

    let descriptors = protox::compile([Path::new(PROTO)], [Path::new(INCLUDE)])
        .map_err(|error| std::io::Error::other(error.to_string()))?;

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_fds(descriptors)
}
