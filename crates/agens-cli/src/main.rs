use std::process::ExitCode;

use agens_core::HeadlessTurnCancellation;

/// What the process exits with when it is signalled a second time. The shell's
/// convention for a process terminated by a signal, and the one a supervisor
/// reads as "it did not stop on its own".
const TERMINATED: u8 = 143;

fn main() -> ExitCode {
    let cancellation = HeadlessTurnCancellation::new();
    let signal_cancellation = cancellation.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build();
        if let Ok(runtime) = runtime {
            runtime.block_on(async { watch_for_a_stop(&signal_cancellation).await });
        }
    });

    let result = agens::execute_os_with_cancellation(
        std::env::args_os().skip(1),
        &agens::CliDependencies::production(),
        &cancellation,
    );

    if !result.stdout.is_empty() {
        print!("{}", result.stdout);
    }
    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr);
    }

    ExitCode::from(result.status.code())
}

/// Turns the two stop signals into the cancellation every loop in the process
/// already watches, and keeps watching after the first one.
///
/// `SIGTERM` is here because that is what a `serve stop`, a process supervisor
/// and a shutting-down machine all send, and the default action for it is to
/// kill the process where it stands — with sessions mid-turn and a worktree
/// half-provisioned. Handling it is what makes the daemon's bounded shutdown
/// reachable from outside the process.
///
/// The second signal is not another request for the same shutdown: it is the
/// answer to a shutdown that is taking too long, and the caller who sent it is
/// entitled to have the process gone.
async fn watch_for_a_stop(cancellation: &HeadlessTurnCancellation) {
    let Ok(mut terminate) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    else {
        // Without a `SIGTERM` handler the default action stands, which is the
        // behaviour this process had before. The interrupt is still worth
        // watching on its own.
        if tokio::signal::ctrl_c().await.is_ok() {
            cancellation.cancel();
        }

        return;
    };

    loop {
        tokio::select! {
            interrupted = tokio::signal::ctrl_c() => {
                if interrupted.is_err() {
                    return;
                }
            }
            _ = terminate.recv() => {}
        }

        if cancellation.is_cancelled() {
            std::process::exit(i32::from(TERMINATED));
        }

        cancellation.cancel();
    }
}
