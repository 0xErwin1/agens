use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "success".into());
    if mode == "descendant" {
        std::thread::sleep(std::time::Duration::from_secs(5));
        return;
    }
    if mode == "no-read-stdin" {
        #[cfg(unix)]
        let pipe_size = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_SETPIPE_SZ, 4096) };
        #[cfg(not(unix))]
        let pipe_size = 4096;
        std::fs::write(
            std::env::args()
                .nth(2)
                .expect("no-read stdin readiness path should be supplied"),
            pipe_size.to_string(),
        )
        .expect("no-read stdin child should signal readiness");
        if let Some(blocked_path) = std::env::args().nth(3) {
            #[cfg(unix)]
            loop {
                let mut buffered = 0;
                let result =
                    unsafe { libc::ioctl(libc::STDIN_FILENO, libc::FIONREAD, &mut buffered) };
                assert_eq!(result, 0, "stdin pipe occupancy should be observable");
                if buffered >= pipe_size {
                    std::fs::write(blocked_path, "stdin pipe is full")
                        .expect("no-read stdin child should signal a full pipe");
                    break;
                }
                std::thread::yield_now();
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
        return;
    }

    let descendant_pid_path = std::env::args().nth(2);
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let request: Value =
            match line.and_then(|line| serde_json::from_str(&line).map_err(io::Error::other)) {
                Ok(request) => request,
                Err(_) => return,
            };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        if mode == "crash" {
            std::process::exit(9);
        }
        if mode.starts_with("descendant-") {
            let descendant = std::process::Command::new(
                std::env::current_exe().expect("fake MCP executable path should be available"),
            )
            .arg("descendant")
            .spawn()
            .expect("fake MCP descendant should start");
            std::fs::write(
                descendant_pid_path
                    .as_deref()
                    .expect("descendant PID path should be supplied"),
                descendant.id().to_string(),
            )
            .expect("descendant PID should be recorded");
            std::mem::forget(descendant);

            if mode == "descendant-crash" {
                std::process::exit(9);
            }
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
        if mode == "sleep" {
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
        if mode == "stderr-flood" {
            let _ = io::stderr().write_all(&vec![b'x'; 128 * 1024]);
        }
        if let Some(secret) = std::env::var_os("FAKE_MCP_STDERR_SECRET") {
            let _ = writeln!(io::stderr(), "{secret:?}");
        }
        if mode == "malformed" {
            let frame =
                std::env::var("FAKE_MCP_PROTOCOL_SECRET").unwrap_or_else(|_| "not-json".into());
            let _ = writeln!(stdout, "{frame}");
            let _ = stdout.flush();
            continue;
        }
        if mode == "oversize" {
            let _ = writeln!(stdout, "{}", "x".repeat(1024 * 1024 + 1));
            let _ = stdout.flush();
            continue;
        }
        if mode == "unterminated-oversize" {
            let _ = stdout.write_all(&vec![b'x'; 1024 * 1024 + 1]);
            let _ = stdout.flush();
            continue;
        }
        let response_id = if mode == "id-mismatch" {
            json!(999)
        } else {
            id
        };
        let response = match request.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                json!({"jsonrpc":"2.0","id":response_id,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}}}})
            }
            Some("tools/list")
                if request
                    .get("params")
                    .and_then(|params| params.get("cursor"))
                    .and_then(Value::as_str)
                    .is_none() =>
            {
                json!({"jsonrpc":"2.0","id":response_id,"result":{"tools":[{"name":"first","description":"Read the fake MCP fixture","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}}],"nextCursor":"next"}})
            }
            Some("tools/list") => {
                json!({"jsonrpc":"2.0","id":response_id,"result":{"tools":[{"name":"second","description":"Write the fake MCP fixture","inputSchema":{"type":"object"}}]}})
            }
            Some("tools/call") if mode == "call-error" => {
                let text = std::env::var("FAKE_MCP_TOOL_ERROR_SECRET")
                    .unwrap_or_else(|_| "tool failed".into());
                json!({"jsonrpc":"2.0","id":response_id,"result":{"content":[{"type":"text","text":text}],"isError":true}})
            }
            Some("tools/call") => {
                if let Some(path) = std::env::var_os("FAKE_MCP_CALL_READY") {
                    std::fs::write(path, "called")
                        .expect("fake MCP call readiness should be recorded");
                }
                if mode == "call-sleep" {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                }
                json!({"jsonrpc":"2.0","id":response_id,"result":{"content":[{"type":"text","text":"tool succeeded"}]}})
            }
            _ => continue,
        };
        let _ = writeln!(stdout, "{response}");
        let _ = stdout.flush();
    }
}
