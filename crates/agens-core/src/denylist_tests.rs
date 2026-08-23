//! What the hard denylist classifies, and what it deliberately leaves alone.
//!
//! Every case here is a command or a path rather than a description of one:
//! the matcher is only worth what it catches in the text a worker actually
//! produces.

use crate::denylist::{Denylist, DenylistClass};
use crate::{PermissionRequest, ToolAccess};

const WORKTREE: &str = "/work/runs/42";

fn denylist() -> Denylist {
    Denylist::new(WORKTREE)
}

fn command(line: &str) -> Option<DenylistClass> {
    denylist().classify(&PermissionRequest::new(
        "project",
        "bash",
        line,
        ToolAccess::Write,
    ))
}

fn write(path: &str) -> Option<DenylistClass> {
    denylist().classify(&PermissionRequest::new(
        "project",
        "write",
        path,
        ToolAccess::Write,
    ))
}

fn escapes(tool: &str, path: &str) -> bool {
    denylist().escapes_worktree(&PermissionRequest::new(
        "project",
        tool,
        path,
        ToolAccess::Write,
    ))
}

#[test]
fn ordinary_work_inside_the_worktree_is_not_denylisted() {
    for line in [
        "cargo test --workspace",
        "git status",
        "git commit -m 'work'",
        "rm -rf target/debug",
        "./scripts/build.sh",
        "grep -rn needle crates/",
    ] {
        assert_eq!(command(line), None, "{line} must stay an ordinary call");
    }

    assert_eq!(write("crates/agens-core/src/lib.rs"), None);
}

#[test]
fn publishing_to_a_remote_is_denylisted_however_it_is_chained() {
    for line in [
        "git push",
        "git push --force origin main",
        "cargo test && git push",
        "sh -c 'git push origin head'",
        "/usr/bin/git push",
    ] {
        assert_eq!(
            command(line),
            Some(DenylistClass::GitPush),
            "{line} must be classified as a push"
        );
    }
}

#[test]
fn escalating_privilege_is_denylisted_before_the_command_it_wraps() {
    for line in ["sudo rm -rf /", "doas apt install curl", "pkexec whoami"] {
        assert_eq!(
            command(line),
            Some(DenylistClass::PrivilegeEscalation),
            "{line} must be classified as escalation"
        );
    }
}

#[test]
fn deleting_outside_the_worktree_is_denylisted_and_deleting_inside_it_is_not() {
    assert_eq!(
        command("rm -rf /work/runs/41"),
        Some(DenylistClass::DeletionOutsideWorktree)
    );
    assert_eq!(
        command("rm ../../other/file"),
        Some(DenylistClass::DeletionOutsideWorktree)
    );
    assert_eq!(command("rm -rf ./target/debug"), None);
    assert_eq!(command("rm crates/agens-core/src/scratch.rs"), None);
}

#[test]
fn reaching_a_credential_is_denylisted_whatever_names_it() {
    assert_eq!(write(".env"), Some(DenylistClass::SecretsAccess));
    assert_eq!(write(".env.production"), Some(DenylistClass::SecretsAccess));
    assert_eq!(
        write("certs/server.pem"),
        Some(DenylistClass::SecretsAccess)
    );
    assert_eq!(
        command("cat ~/.ssh/id_ed25519"),
        Some(DenylistClass::SecretsAccess)
    );
    assert_eq!(
        command("cp ./deploy/credentials /tmp/x"),
        Some(DenylistClass::SecretsAccess)
    );
}

#[test]
fn an_irreversible_operation_is_denylisted_by_what_it_is() {
    for line in [
        "terraform apply -auto-approve",
        "kubectl delete pods --all",
        "dd if=/dev/zero of=/dev/sda",
        "git reset --hard origin/main",
        "psql -c 'DROP TABLE users'",
        "psql -c 'DELETE FROM users'",
        "npx prisma migrate deploy",
        "pg_restore -d app dump.sql",
        "aws dynamodb delete-table --table-name app",
    ] {
        assert_eq!(
            command(line),
            Some(DenylistClass::IrreversibleOperation),
            "{line} must be classified as irreversible"
        );
    }

    assert_eq!(command("psql -c 'DELETE FROM users WHERE id = 1'"), None);
}

#[test]
fn stopping_the_server_that_hosts_the_run_is_denylisted() {
    for line in [
        "agens team stop",
        "agens serve restart",
        "systemctl --user stop agens.service",
        "pkill -f agens",
    ] {
        assert_eq!(
            command(line),
            Some(DenylistClass::ServerLifecycle),
            "{line} must be classified as server lifecycle"
        );
    }

    assert_eq!(
        write("/etc/systemd/user/agens.service"),
        Some(DenylistClass::ServerLifecycle)
    );
}

#[test]
fn reaching_outside_the_worktree_is_out_of_scope_for_a_tool_and_for_a_command() {
    assert_eq!(
        write("/work/runs/41/src/lib.rs"),
        Some(DenylistClass::OutOfScope)
    );
    assert_eq!(write("../../../etc/hosts"), Some(DenylistClass::OutOfScope));
    assert_eq!(
        command("cp ./out.txt /work/runs/41/out.txt"),
        Some(DenylistClass::OutOfScope)
    );
}

/// `bash` runs the whole line with the worktree as its working directory, so a
/// `cd` inside the line is what decides where every operand after it lands. A
/// line that leaves the worktree has left it whatever it does next, and the
/// operands of a line that stays inside are read from where they were written.
#[test]
fn a_directory_change_inside_a_command_line_moves_the_operands_that_follow_it() {
    for line in [
        "cd .. && rm -rf victim",
        "cd ../.. && sqlite3 control.db \"UPDATE runs SET state = 'done'\"",
        "cd ../.. && cat x >> worktree-policy.toml",
        "cd /tmp && rm -rf victim",
        "cd && rm -rf victim",
        "cd ~ && rm -rf victim",
        "cd - && rm -rf victim",
        "cd src/.. && cd .. && cargo run",
    ] {
        assert_eq!(
            command(line),
            Some(DenylistClass::OutOfScope),
            "{line} leaves the worktree"
        );
    }

    assert_eq!(command("cd crates/agens-core && cargo test"), None);
    // A directory change is still an invocation, and what it names is still
    // read: moving into a credential store is reaching one.
    assert_eq!(
        command("cd ./deploy/.aws && cat config"),
        Some(DenylistClass::SecretsAccess)
    );
    assert_eq!(command("cd ./src && rm -rf ../target"), None);
    assert_eq!(
        command("cd src && rm -rf ../../victim"),
        Some(DenylistClass::DeletionOutsideWorktree)
    );
}

/// An argument the matcher cannot read as a program is still read for the
/// paths written inside it. Joining the whole argument onto the working
/// directory instead resolves to something the worktree contains, which is the
/// opposite of what the argument does.
#[test]
fn a_path_written_inside_an_interpreter_argument_is_judged_as_a_path() {
    assert_eq!(
        command("python -c \"import os; os.remove('/etc/passwd')\""),
        Some(DenylistClass::OutOfScope)
    );
    assert_eq!(
        command("python3 -c 'open(\"/work/runs/41/out\", \"w\")'"),
        Some(DenylistClass::OutOfScope)
    );
    assert_eq!(
        command("node -e \"require('fs').readFileSync('/home/worker/.ssh/id_rsa')\""),
        Some(DenylistClass::SecretsAccess)
    );
    assert_eq!(
        command("rm -rf /work/runs/41/*"),
        Some(DenylistClass::DeletionOutsideWorktree)
    );

    // A program that only names paths the worktree contains stays ordinary.
    assert_eq!(
        command("python -c \"import os; os.remove('target/debug/x')\""),
        None
    );
    assert_eq!(command("echo 'a && b'"), None);
}

/// A sub-agent launch is one more call through the same dispatcher, so the
/// class it is stopped with is the class its own target earns. Delegating buys
/// nothing the caller did not already have, and it costs nothing either.
#[test]
fn a_delegation_is_classified_by_what_it_reaches() {
    let task = PermissionRequest::new("project", "task", "review the diff", ToolAccess::Write);

    assert_eq!(denylist().classify(&task), None);
}

#[test]
fn only_a_path_shaped_target_reaches_the_confinement_floor() {
    assert!(escapes("write", "/etc/hosts"));
    assert!(escapes("read", "../../other/file"));
    assert!(!escapes("write", "src/lib.rs"));
    // A command line is operands rather than one path, so a reading of it must
    // never reach a hard deny.
    assert!(!escapes("bash", "rm -rf /etc"));
}

#[test]
fn every_class_carries_a_distinct_identifier() {
    let classes = [
        DenylistClass::GitPush,
        DenylistClass::DeletionOutsideWorktree,
        DenylistClass::PrivilegeEscalation,
        DenylistClass::SecretsAccess,
        DenylistClass::IrreversibleOperation,
        DenylistClass::ServerLifecycle,
        DenylistClass::OutOfScope,
    ];
    let ids = classes.map(DenylistClass::id);
    let unique = ids.iter().collect::<std::collections::BTreeSet<_>>();

    assert_eq!(unique.len(), classes.len());
    assert!(classes.iter().all(|class| !class.headline().is_empty()));
}

/// A relative path carrying a glob, a brace expansion or a variable is still
/// the path it names. Those characters stop the argument from reading as one
/// path, so what the argument names has to be read as the paths written inside
/// it or the escape goes unseen.
#[test]
fn a_relative_path_written_with_shell_syntax_is_judged_as_a_path() {
    assert_eq!(
        command("rm -rf ../victim/*"),
        Some(DenylistClass::DeletionOutsideWorktree)
    );
    assert_eq!(
        command("rm -rf ../{a,b}"),
        Some(DenylistClass::DeletionOutsideWorktree)
    );
    assert_eq!(
        command("cat $HOME/.ssh/id_rsa"),
        Some(DenylistClass::SecretsAccess)
    );
    assert_eq!(
        command("cat ~/.ssh/id_rsa*"),
        Some(DenylistClass::SecretsAccess)
    );

    // The same syntax over something the worktree contains stays ordinary.
    assert_eq!(command("rm -rf target/{debug,release}"), None);
    assert_eq!(command("rm -rf ./target/debug/*"), None);
}

/// Every way a line moves its own working directory, not only the one spelling
/// the matcher can resolve. A move it cannot resolve is a move out: reading it
/// as staying put leaves every operand after it judged against a directory the
/// line is no longer in.
#[test]
fn a_directory_change_the_matcher_cannot_resolve_is_read_as_leaving_the_worktree() {
    for line in [
        "pushd .. && rm -rf victim",
        "pushd ../.. && cargo run",
        "popd && rm -rf victim",
        "cd \"$HOME\" && rm -rf victim",
        "cd $PARENT && rm -rf victim",
        "cd `dirname $PWD` && rm -rf victim",
        "cd ~worker && rm -rf victim",
    ] {
        assert_eq!(
            command(line),
            Some(DenylistClass::OutOfScope),
            "{line} leaves the worktree"
        );
    }

    assert_eq!(command("pushd crates/agens-core && cargo test"), None);
}

/// `eval` runs the line written inside it, so the matcher has to read that line
/// rather than the opaque argument carrying it.
#[test]
fn a_line_running_through_eval_is_read_as_the_line_it_runs() {
    assert_eq!(
        command("eval \"cd .. && rm -rf x\""),
        Some(DenylistClass::OutOfScope)
    );
    assert_eq!(
        command("eval 'git push origin main'"),
        Some(DenylistClass::GitPush)
    );
    assert_eq!(
        command("eval \"cat /home/worker/.ssh/id_rsa\""),
        Some(DenylistClass::SecretsAccess)
    );

    assert_eq!(command("eval 'cargo test --workspace'"), None);
}
