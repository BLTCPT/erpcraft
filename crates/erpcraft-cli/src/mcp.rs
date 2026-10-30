//! `erpcraft mcp` — ERPCraft as an MCP server on stdio.
//!
//! Newline-delimited JSON-RPC 2.0, the standard MCP stdio transport
//! (protocol 2024-11-05), mirroring the `bt mcp` precedent. The server
//! is a thin protocol shell: every tool routes into the same dispatch
//! table the CLI uses (`commands::dispatch`), so the two surfaces can
//! never drift. Diagnostics go to stderr; stdout carries protocol
//! frames only.
//!
//! Tool failures (refusals, bad arguments, core errors) come back as
//! `isError: true` content carrying the dispatch error as JSON — the
//! message, plus the configurator's structured explanations and the
//! evaluation when the refusal carries them. Protocol errors use the
//! JSON-RPC error object.

use crate::commands::{all_specs, dispatch, open_door_db, DispatchError, Door};
use erpcraft_core::rusqlite::Connection;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::process::ExitCode;

const PROTOCOL_VERSION: &str = "2024-11-05";

fn tool_definitions() -> Value {
    Value::Array(
        all_specs()
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "description": s.summary,
                    "inputSchema": s.input_schema,
                })
            })
            .collect(),
    )
}

fn call_tool(conn: &mut Connection, name: &str, args: &Value) -> Result<Value, DispatchError> {
    dispatch(conn, Door::Mcp, name, args)
}

/// Handle one JSON-RPC message. Returns the response frame, or `None`
/// for notifications (which are never answered).
pub fn handle_message(conn: &mut Connection, msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = msg.get("id").cloned();
    let is_request = matches!(&id, Some(v) if !v.is_null());

    let result: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {
                "name": "erpcraft",
                "version": env!("CARGO_PKG_VERSION"),
            },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match call_tool(conn, name, &args) {
                Ok(value) => Ok(json!({
                    "content": [{
                        "type": "text",
                        "text": serde_json::to_string_pretty(&value)
                            .unwrap_or_else(|_| "{}".to_string()),
                    }],
                    "isError": false,
                })),
                Err(e) => Ok(json!({
                    "content": [{
                        "type": "text",
                        "text": serde_json::to_string(&e.to_json())
                            .unwrap_or_else(|_| "{}".to_string()),
                    }],
                    "isError": true,
                })),
            }
        }
        _ => Err((-32601, format!("method not found: {method}"))),
    };

    if !is_request {
        return None;
    }
    let id = id.unwrap_or(Value::Null);
    Some(match result {
        Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }),
    })
}

/// Serve MCP on stdio until stdin closes. The database opens once, at
/// startup, exactly as the other doors open it.
pub fn serve() -> ExitCode {
    let mut conn = match open_door_db() {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("erpcraft mcp: {}", e.message);
            return ExitCode::from(2);
        }
    };
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut stdout = std::io::stdout().lock();
    loop {
        let line = match lines.next() {
            Some(Ok(line)) => line,
            Some(Err(e)) => {
                eprintln!("erpcraft mcp: stdin error: {e}");
                return ExitCode::from(2);
            }
            None => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let frame = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32700, "message": format!("parse error: {e}") },
                });
                if writeln!(stdout, "{frame}").is_err() || stdout.flush().is_err() {
                    break;
                }
                continue;
            }
        };
        if let Some(frame) = handle_message(&mut conn, &msg) {
            if writeln!(stdout, "{frame}").is_err() || stdout.flush().is_err() {
                break;
            }
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use erpcraft_core::open_db;

    fn memory_conn() -> Connection {
        open_db(":memory:").unwrap()
    }

    fn call(conn: &mut Connection, name: &str, arguments: Value) -> Value {
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        });
        handle_message(conn, &msg).expect("a request gets a response")
    }

    fn result_text(result: &Value) -> Value {
        let text = result["result"]["content"][0]["text"]
            .as_str()
            .expect("text content");
        serde_json::from_str(text).expect("tool text is JSON")
    }

    #[test]
    fn initialize_reports_the_server_identity() {
        let mut conn = memory_conn();
        let msg = json!({ "jsonrpc": "2.0", "id": 7, "method": "initialize", "params": {} });
        let resp = handle_message(&mut conn, &msg).unwrap();
        assert_eq!(resp["id"], 7);
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(resp["result"]["serverInfo"]["name"], "erpcraft");
    }

    #[test]
    fn notifications_get_no_response() {
        let mut conn = memory_conn();
        let msg = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle_message(&mut conn, &msg).is_none());
    }

    #[test]
    fn unknown_methods_get_a_json_rpc_error() {
        let mut conn = memory_conn();
        let msg = json!({ "jsonrpc": "2.0", "id": 3, "method": "nope/nope" });
        let resp = handle_message(&mut conn, &msg).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn tools_list_matches_the_dispatch_table() {
        let mut conn = memory_conn();
        let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
        let resp = handle_message(&mut conn, &msg).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), all_specs().len());
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in [
            "create_quote",
            "create_invoice",
            "issue_invoice",
            "record_return",
            "evaluate_config",
            "buy_materials",
        ] {
            assert!(names.contains(&expected), "tool {expected} is listed");
        }
        // create_invoice and issue_invoice are two tools, never one —
        // Day 5's agent needs the draft visible before it posts.
        assert!(names.contains(&"create_invoice") && names.contains(&"issue_invoice"));
    }

    #[test]
    fn unknown_tools_are_tool_errors_not_protocol_errors() {
        let mut conn = memory_conn();
        let resp = call(&mut conn, "no_such_tool", json!({}));
        assert_eq!(resp["result"]["isError"], true);
        let payload = result_text(&resp);
        assert!(payload["error"]
            .as_str()
            .unwrap()
            .contains("unknown command"));
    }

    #[test]
    fn a_tool_call_routes_into_the_same_dispatch() {
        let mut conn = memory_conn();
        let resp = call(&mut conn, "health", json!({}));
        assert_eq!(resp["result"]["isError"], false);
        let health = result_text(&resp);
        assert_eq!(health["status"], "ok");
        assert_eq!(health["accounts"], 22);
        assert_eq!(health["journal_entries"], 1, "just the opening entry");
    }
}
