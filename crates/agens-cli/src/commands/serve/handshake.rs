//! The attach handshake: what a launch does about the daemon already serving.
//!
//! Daemons outlive the binaries that start them, so every attach begins by
//! asking the process on the socket what it is. Two comparisons follow, making
//! two different decisions. The wire revision decides *compatibility*: a
//! daemon on another revision answers this client wrongly rather than
//! refusing it, so attaching to one is refused here instead. The build stamp
//! decides *freshness*: a compatible daemon from another build keeps serving,
//! and is replaced by the binary in hand the first time a default launch
//! finds it with nothing running — the same launch that would have started a
//! daemon anyway. An explicit attach never restarts anything.

use agens_bootstrap::Bootstrap;
use agens_coordinator_client::{ClientError, Coordinator, DaemonBuild};
use agens_error::CliError;
use agens_store::{ControlPlaneStore, QuestionStore};

use super::lifecycle;

/// What a launch may do to a daemon whose build differs from this client's.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SkewPolicy {
    /// The default launch: it starts daemons, so it may also replace one that
    /// is running nothing.
    RestartWhenIdle,
    /// An explicit attach: it reports skew and touches nothing.
    ReportOnly,
}

/// What the handshake concluded.
#[derive(Clone, Debug, Eq, PartialEq)]
enum SkewDecision {
    /// Attach, with a one-line notice when the daemon is a different but
    /// compatible build.
    Proceed(Option<String>),
    /// Stop the daemon and start one from this binary, then attach.
    Replace(String),
    /// Do not attach; the message says why and what to do.
    Refuse(String),
}

/// This client's side of the handshake.
struct ClientIdentity {
    wire_revision: u64,
    build: String,
}

impl ClientIdentity {
    fn this_binary() -> Self {
        Self {
            wire_revision: agens_server::identity::WIRE_REVISION,
            build: agens_server::identity::BUILD_STAMP.to_owned(),
        }
    }
}

/// Checks the daemon's build before a client opens anything against it, and
/// acts on the conclusion: attaches, replaces an idle daemon, or refuses with
/// the remedy.
///
/// Returns the notice a launch should surface, when there is one. No daemon on
/// the socket is nothing to check: whatever the launch does next will say so.
pub(crate) fn check(bootstrap: &Bootstrap, policy: SkewPolicy) -> Result<Option<String>, CliError> {
    let daemon = match probe(bootstrap) {
        Probe::NoDaemon => return Ok(None),
        Probe::PredatesHandshake => None,
        Probe::Answered(build) => Some(build),
    };

    let activity = activity(bootstrap, &daemon);

    match decide(
        &ClientIdentity::this_binary(),
        daemon.as_ref(),
        activity.as_deref(),
        policy,
    ) {
        SkewDecision::Proceed(notice) => Ok(notice),
        SkewDecision::Replace(notice) => {
            lifecycle::stop(bootstrap)?;
            lifecycle::start_detached(bootstrap)?;

            Ok(Some(notice))
        }
        SkewDecision::Refuse(report) => Err(CliError::unavailable(report)),
    }
}

enum Probe {
    NoDaemon,
    /// Something answered the socket but not the handshake: an older daemon,
    /// or one whose answer this client cannot read. Either way its build is
    /// unknowable, which the decision treats as incompatible.
    PredatesHandshake,
    Answered(DaemonBuild),
}

/// Asks the process on the socket what it is.
fn probe(bootstrap: &Bootstrap) -> Probe {
    let socket = agens_server::socket_path(bootstrap.data_directory());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build();
    let Ok(runtime) = runtime else {
        return Probe::NoDaemon;
    };

    let asked = runtime.block_on(async {
        let coordinator = Coordinator::attach(&socket).await?;

        coordinator.chat().build_info().await
    });

    match asked {
        Ok(Some(build)) => Probe::Answered(build),
        Ok(None) => Probe::PredatesHandshake,
        Err(ClientError::NotRunning(_)) => Probe::NoDaemon,
        Err(_) => Probe::PredatesHandshake,
    }
}

/// What the daemon is in the middle of, or `None` when it is idle.
///
/// Runs and open questions are read from the shared control plane, the way
/// `serve status` reads them; answering chats only the daemon knows, so they
/// come from its handshake answer. A daemon that could not answer the
/// handshake has unknowable chats, which is why it is never restarted and its
/// activity is never consulted.
fn activity(bootstrap: &Bootstrap, daemon: &Option<DaemonBuild>) -> Option<String> {
    let mut held = Vec::new();

    if let Some(build) = daemon
        && build.answering_chats > 0
    {
        held.push(format!("{} answering chats", build.answering_chats));
    }

    let data_directory = bootstrap.data_directory();
    if agens_store::unified_database_path(data_directory).exists() {
        if let Some(runs) = active_runs(data_directory)
            && runs > 0
        {
            held.push(format!("{runs} active runs"));
        }

        if let Some(questions) = open_questions(data_directory)
            && questions > 0
        {
            held.push(format!("{questions} open questions"));
        }
    }

    if held.is_empty() {
        return None;
    }

    Some(held.join(", "))
}

/// The runs the daemon is carrying. `None` when the journal is unreadable,
/// which the caller must treat as busy: an unreadable journal proves nothing
/// idle.
fn active_runs(data_directory: &std::path::Path) -> Option<usize> {
    let store = ControlPlaneStore::open(data_directory).ok()?;

    let mut total = 0;
    for state in lifecycle::ACTIVE_STATES {
        total += store.runs_in_state(state).ok()?.len();
    }

    Some(total)
}

fn open_questions(data_directory: &std::path::Path) -> Option<usize> {
    let store = QuestionStore::open(data_directory).ok()?;

    Some(store.open_questions().ok()?.len())
}

/// The decision table, pure so the matrix is provable without a daemon.
fn decide(
    client: &ClientIdentity,
    daemon: Option<&DaemonBuild>,
    activity: Option<&str>,
    policy: SkewPolicy,
) -> SkewDecision {
    let Some(daemon) = daemon else {
        return SkewDecision::Refuse(format!(
            "the daemon on this machine predates the version handshake and cannot serve this \
             client ({}); run `agens serve stop`, then launch again",
            client.build
        ));
    };

    let compatible = daemon.wire_revision == client.wire_revision;
    let same_build = daemon.build == client.build;
    let idle = activity.is_none();
    let may_replace = policy == SkewPolicy::RestartWhenIdle && idle;

    if compatible && same_build {
        return SkewDecision::Proceed(None);
    }

    if may_replace {
        return SkewDecision::Replace(format!(
            "replaced the idle daemon built as {} with this build ({})",
            daemon.build, client.build
        ));
    }

    if compatible {
        return SkewDecision::Proceed(Some(format!(
            "the daemon was built as {} and this client as {}; they are compatible, so it keeps \
             serving — a later launch that finds it idle will replace it, or run `agens serve \
             stop` to update it now",
            daemon.build, client.build
        )));
    }

    let held = activity.map_or_else(String::new, |activity| format!(" it is busy ({activity});"));

    SkewDecision::Refuse(format!(
        "the daemon was built as {} and no longer speaks this client's wire ({});{held} finish or \
         stop its work, then run `agens serve stop` and launch again",
        daemon.build,
        client.build,
        held = held
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> ClientIdentity {
        ClientIdentity {
            wire_revision: 3,
            build: "0.1.0+client99".to_owned(),
        }
    }

    fn daemon(wire_revision: u64, build: &str) -> DaemonBuild {
        DaemonBuild {
            wire_revision,
            build: build.to_owned(),
            answering_chats: 0,
        }
    }

    #[test]
    fn a_daemon_of_the_same_build_is_attached_to_silently() {
        let same = daemon(3, "0.1.0+client99");

        for policy in [SkewPolicy::RestartWhenIdle, SkewPolicy::ReportOnly] {
            assert_eq!(
                decide(&client(), Some(&same), None, policy),
                SkewDecision::Proceed(None)
            );
        }
    }

    #[test]
    fn a_compatible_older_daemon_that_is_idle_is_replaced_by_a_default_launch() {
        let older = daemon(3, "0.1.0+daemon11");

        let decision = decide(&client(), Some(&older), None, SkewPolicy::RestartWhenIdle);

        let SkewDecision::Replace(notice) = decision else {
            panic!("an idle compatible daemon of another build is replaced: {decision:?}");
        };
        assert!(notice.contains("0.1.0+daemon11"), "{notice}");
        assert!(notice.contains("0.1.0+client99"), "{notice}");
    }

    #[test]
    fn a_compatible_older_daemon_that_is_busy_keeps_serving_with_a_notice() {
        let older = daemon(3, "0.1.0+daemon11");

        let decision = decide(
            &client(),
            Some(&older),
            Some("1 answering chat"),
            SkewPolicy::RestartWhenIdle,
        );

        let SkewDecision::Proceed(Some(notice)) = decision else {
            panic!("a busy compatible daemon is served with, not killed: {decision:?}");
        };
        assert!(notice.contains("0.1.0+daemon11"), "{notice}");
        assert!(notice.contains("0.1.0+client99"), "{notice}");
    }

    /// Explicit `--attach` never restarts anything, however idle the daemon.
    #[test]
    fn an_explicit_attach_serves_with_a_compatible_older_daemon_without_replacing_it() {
        let older = daemon(3, "0.1.0+daemon11");

        let decision = decide(&client(), Some(&older), None, SkewPolicy::ReportOnly);

        assert!(
            matches!(decision, SkewDecision::Proceed(Some(_))),
            "{decision:?}"
        );
    }

    #[test]
    fn an_incompatible_idle_daemon_is_replaced_by_a_default_launch() {
        let incompatible = daemon(2, "0.1.0+daemon11");

        let decision = decide(
            &client(),
            Some(&incompatible),
            None,
            SkewPolicy::RestartWhenIdle,
        );

        assert!(matches!(decision, SkewDecision::Replace(_)), "{decision:?}");
    }

    #[test]
    fn an_incompatible_busy_daemon_is_reported_and_left_running() {
        let incompatible = daemon(2, "0.1.0+daemon11");

        let decision = decide(
            &client(),
            Some(&incompatible),
            Some("2 active runs"),
            SkewPolicy::RestartWhenIdle,
        );

        let SkewDecision::Refuse(report) = decision else {
            panic!("a busy incompatible daemon is refused, never killed: {decision:?}");
        };
        assert!(report.contains("0.1.0+daemon11"), "{report}");
        assert!(report.contains("0.1.0+client99"), "{report}");
        assert!(report.contains("2 active runs"), "{report}");
        assert!(report.contains("agens serve stop"), "{report}");
    }

    #[test]
    fn an_explicit_attach_refuses_an_incompatible_daemon_even_when_it_is_idle() {
        let incompatible = daemon(2, "0.1.0+daemon11");

        let decision = decide(&client(), Some(&incompatible), None, SkewPolicy::ReportOnly);

        let SkewDecision::Refuse(report) = decision else {
            panic!("--attach reports, it never restarts: {decision:?}");
        };
        assert!(report.contains("agens serve stop"), "{report}");
    }

    /// A daemon that predates the handshake cannot say whether it is answering
    /// somebody, so no launch form may kill it on a guess.
    #[test]
    fn a_daemon_that_predates_the_handshake_is_reported_and_never_restarted() {
        for policy in [SkewPolicy::RestartWhenIdle, SkewPolicy::ReportOnly] {
            let decision = decide(&client(), None, None, policy);

            let SkewDecision::Refuse(report) = decision else {
                panic!("a pre-handshake daemon is refused, never killed: {decision:?}");
            };
            assert!(report.contains("agens serve stop"), "{report}");
            assert!(report.contains("0.1.0+client99"), "{report}");
        }
    }
}
