use std::process::ExitCode;

fn main() -> ExitCode {
    let result = agens::execute(
        std::env::args().skip(1),
        &agens::CliDependencies::production(),
    );

    if !result.stdout.is_empty() {
        print!("{}", result.stdout);
    }
    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr);
    }

    ExitCode::from(result.status.code())
}
