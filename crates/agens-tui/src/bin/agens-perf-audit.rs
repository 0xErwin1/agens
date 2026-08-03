//! Entry point for the performance audit.
//!
//! Deliberately thin: everything it drives lives in `agens_tui::perf`, because
//! the render-skip gate the audit measures is private to that crate and a
//! binary that reimplemented it would be measuring its own reimplementation.

use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage:
  agens-perf-audit run <trace-dir> <run-id>
  agens-perf-audit diff <base.jsonl> <new.jsonl>";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let borrowed: Vec<&str> = arguments.iter().map(String::as_str).collect();

    match borrowed.as_slice() {
        ["run", directory, run_id] => run(Path::new(directory), run_id),
        ["diff", base, new] => diff(Path::new(base), Path::new(new)),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn run(directory: &Path, run_id: &str) -> ExitCode {
    let outcome = match agens_tui::perf::run_all(directory, run_id) {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("audit did not run: {error}");
            return ExitCode::FAILURE;
        }
    };

    println!("canonical trace: {}", outcome.paths.jsonl.display());
    if let Some(chrome) = &outcome.paths.chrome {
        println!("chrome trace:    {}", chrome.display());
    }

    if outcome.failed.is_empty() {
        return ExitCode::SUCCESS;
    }

    eprintln!(
        "incomplete trace: {} did not finish",
        outcome.failed.join(", ")
    );
    ExitCode::FAILURE
}

fn diff(base: &Path, new: &Path) -> ExitCode {
    let base_records = match read(base) {
        Ok(records) => records,
        Err(code) => return code,
    };
    let new_records = match read(new) {
        Ok(records) => records,
        Err(code) => return code,
    };

    match agens_perf::compare(base_records, new_records) {
        Ok(report) => {
            print!("{}", agens_perf::render_text(&report));
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("traces are not comparable: {error}");
            ExitCode::FAILURE
        }
    }
}

fn read(path: &Path) -> Result<Vec<agens_perf::Record>, ExitCode> {
    agens_perf::read_trace(path).map_err(|error| {
        eprintln!("could not read {}: {error}", path.display());
        ExitCode::FAILURE
    })
}
