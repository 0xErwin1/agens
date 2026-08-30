//! What this binary is, as the attach handshake reports it.
//!
//! Two facts, because the handshake makes two different decisions with them.
//! The wire revision decides *compatibility*: whether a daemon built from
//! other sources still serves this client's contract, which is what protects a
//! client from the store errors an incompatible daemon answers with. The build
//! stamp decides *freshness*: whether an idle daemon should be replaced by the
//! binary that is attaching, even when the two are still compatible.

/// The client/daemon contract revision.
///
/// Bumped by hand, and only when the contract between a client and a daemon
/// breaks: a wire or store change an older daemon would answer incorrectly
/// rather than refuse. Adding an RPC or a field does not break it; renumbering,
/// retyping or re-meaning one does.
pub const WIRE_REVISION: u64 = 1;

/// The build this binary came from: the crate version plus the git commit it
/// was compiled at, or `unknown` when the sources were not a git checkout.
///
/// Two stamps differing means the daemon and the client are different builds.
/// It says nothing about direction: a commit hash does not order, so what the
/// handshake does with a difference is replace an *idle* daemon with the
/// binary in hand, never judge which of the two is newer.
pub const BUILD_STAMP: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("AGENS_GIT_COMMIT"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_stamp_carries_the_crate_version_and_a_commit() {
        let (version, commit) = BUILD_STAMP
            .split_once('+')
            .expect("the stamp is version+commit");

        assert_eq!(version, env!("CARGO_PKG_VERSION"));
        assert!(!commit.is_empty(), "the commit half is never empty");
    }
}
