//! Day-4 exit tests 1/3/4 through the real MCP server: spawn
//! `erpcraft mcp` on a temp database and drive the full loop with
//! newline-delimited JSON-RPC over stdio — initialize, tools/list,
//! then quote (configured BRK-100) → send → accept → order → fulfil →
//! invoice → issue → partial payment → refused overpayment → return.
//! Tool failures arrive as isError results carrying the refusal, and
//! the entry count does not move when one does.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Mcp {
    fn spawn(db: &std::path::Path) -> Mcp {
        let mut child = Command::new(env!("CARGO_BIN_EXE_erpcraft"))
            .arg("mcp")
            .env("ERPCRAFT_DB", db)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("erpcraft mcp spawns");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Mcp {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    fn send(&mut self, frame: &Value) {
        writeln!(self.stdin, "{frame}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("the server replies");
        let resp: Value = serde_json::from_str(&line).expect("the reply is JSON");
        assert_eq!(resp["id"], id, "replies arrive in order");
        resp
    }

    /// Call a tool and return (is_error, parsed content payload).
    fn tool(&mut self, name: &str, arguments: Value) -> (bool, Value) {
        let resp = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        let result = &resp["result"];
        let text = result["content"][0]["text"].as_str().expect("text content");
        let payload: Value = serde_json::from_str(text).expect("tool payload is JSON");
        (result["isError"].as_bool().unwrap(), payload)
    }

    fn ok_tool(&mut self, name: &str, arguments: Value) -> Value {
        let (is_error, payload) = self.tool(name, arguments);
        assert!(!is_error, "{name} failed: {payload}");
        payload
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_full_loop_over_stdio() {
    let db = std::env::temp_dir().join(format!("erpcraft-mcp-stdio-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    let mut mcp = Mcp::spawn(&db);

    // initialize: the handshake names the server and the protocol.
    let init = mcp.request("initialize", json!({}));
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init["result"]["serverInfo"]["name"], "erpcraft");
    mcp.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    // tools/list: the dispatch table, with create_invoice and
    // issue_invoice as two distinct tools.
    let list = mcp.request("tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(tools.len(), 43, "every command is a tool");
    assert!(names.contains(&"create_invoice") && names.contains(&"issue_invoice"));
    assert!(names.contains(&"record_return") && names.contains(&"evaluate_config"));

    let health = mcp.ok_tool("health", json!({}));
    assert_eq!(health["journal_entries"], 1);

    // The configured loop (exit tests 1 + 3 through MCP alone).
    let quote = mcp.ok_tool(
        "create_quote",
        json!({
            "customer_id": 1,
            "lines": [{
                "item_id": 1,
                "qty": 50,
                "configuration": { "material": "steel", "finish": "galvanized" }
            }]
        }),
    );
    assert_eq!(quote["lines"][0]["unit_price_cents"], 16_200);
    let quote_id = quote["id"].clone();
    mcp.ok_tool("send_quote", json!({ "quote_id": quote_id }));
    mcp.ok_tool("accept_quote", json!({ "quote_id": quote_id }));
    let order = mcp.ok_tool("create_order", json!({ "quote_id": quote_id }));
    let order_id = order["id"].clone();
    mcp.ok_tool("fulfil_order", json!({ "order_id": order_id }));
    let invoice = mcp.ok_tool("create_invoice", json!({ "order_id": order_id }));
    let invoice_id = invoice["id"].clone();
    assert_eq!(invoice["status"], "draft");

    let issued = mcp.ok_tool("issue_invoice", json!({ "invoice_id": invoice_id }));
    // COGS from the $91.00 snapshot, through the agent door.
    assert_eq!(issued["cogs_entry"]["lines"][0]["debit_cents"], 455_000);
    assert_eq!(issued["revenue_entry"]["lines"][0]["debit_cents"], 810_000);
    let revenue_entry_id = issued["revenue_entry"]["id"].clone();

    // Partial payment posts; the door's own builder entries are
    // tagged "mcp".
    let paid = mcp.ok_tool(
        "record_payment",
        json!({ "invoice_id": invoice_id, "amount_cents": 100_000 }),
    );
    assert_eq!(
        paid["entry"]["source"],
        json!(format!("payment:{}", paid["payment"]["id"]))
    );
    let built = mcp.ok_tool(
        "buy_materials",
        json!({ "amount_cents": 10_000, "entry_date": "2026-10-29" }),
    );
    assert_eq!(built["source"], "mcp");

    // Overpayment is a tool error carrying the core's message, and
    // nothing posts (exit test 4, MCP half).
    let entries_before = mcp.ok_tool("health", json!({}))["journal_entries"].clone();
    let (is_error, payload) = mcp.tool(
        "record_payment",
        json!({ "invoice_id": invoice_id, "amount_cents": 800_000 }),
    );
    assert!(is_error);
    assert!(
        payload["error"]
            .as_str()
            .unwrap()
            .contains("exceeds the balance"),
        "payload: {payload}"
    );
    assert_eq!(
        mcp.ok_tool("health", json!({}))["journal_entries"],
        entries_before,
        "the refused payment posted nothing"
    );

    // The configurator refusal carries explanations + the evaluation.
    let (is_error, payload) = mcp.tool(
        "evaluate_config",
        json!({
            "model_id": 1,
            "configuration": { "material": "aluminum", "finish": "galvanized" }
        }),
    );
    assert!(is_error);
    assert_eq!(payload["evaluation"]["verdict"]["status"], "inconsistent");
    assert!(
        !payload["explanations"].as_array().unwrap().is_empty(),
        "explanations ride along: {payload}"
    );

    // Return 2 units: two reversing entries pointing at issuance.
    let returned = mcp.ok_tool(
        "record_return",
        json!({ "invoice_id": invoice_id, "lines": [{ "item_id": 1, "qty": 2 }] }),
    );
    assert_eq!(returned["revenue_entry"]["lines"][0]["debit_cents"], 32_400);
    assert_eq!(
        returned["revenue_entry"]["reverses_entry_id"],
        revenue_entry_id
    );
    assert_eq!(returned["cogs_entry"]["lines"][0]["debit_cents"], 18_200);

    // And the book still balances.
    let tb = mcp.ok_tool("trial_balance", json!({}));
    assert_eq!(tb["total_debit_cents"], tb["total_credit_cents"]);
}
