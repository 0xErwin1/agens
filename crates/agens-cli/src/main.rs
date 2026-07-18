use std::process::ExitCode;

use agens_core::HeadlessTurnCancellation;

fn main() -> ExitCode {
    let cancellation = HeadlessTurnCancellation::with_deadline(std::time::Duration::from_secs(120));
    let signal_cancellation = cancellation.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build();
        if let Ok(runtime) = runtime {
            runtime.block_on(async {
                if tokio::signal::ctrl_c().await.is_ok() {
                    signal_cancellation.cancel();
                }
            });
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
