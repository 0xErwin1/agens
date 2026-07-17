use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "success".into());
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
        if mode == "sleep" {
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
        if mode == "stderr-flood" {
            let _ = io::stderr().write_all(&vec![b'x'; 128 * 1024]);
        }
        if mode == "malformed" {
            let _ = writeln!(stdout, "not-json");
            let _ = stdout.flush();
            continue;
        }
        if mode == "oversize" {
            let _ = writeln!(stdout, "{}", "x".repeat(1024 * 1024 + 1));
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
                json!({"jsonrpc":"2.0","id":response_id,"result":{"tools":[{"name":"first","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}}],"nextCursor":"next"}})
            }
            Some("tools/list") => {
                json!({"jsonrpc":"2.0","id":response_id,"result":{"tools":[{"name":"second","inputSchema":{"type":"object"}}]}})
            }
            Some("tools/call") if mode == "call-error" => {
                json!({"jsonrpc":"2.0","id":response_id,"result":{"content":[{"type":"text","text":"tool failed"}],"isError":true}})
            }
            Some("tools/call") => {
                json!({"jsonrpc":"2.0","id":response_id,"result":{"content":[{"type":"text","text":"tool succeeded"}]}})
            }
            _ => continue,
        };
        let _ = writeln!(stdout, "{response}");
        let _ = stdout.flush();
    }
}
