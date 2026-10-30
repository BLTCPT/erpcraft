//! Day-4 exit tests 1, 3 and 5 at the dispatch level: the full loop
//! (configured quote → send → accept → order → fulfil → invoice →
//! issue → payment → return, plus a builder and a raw entry) driven
//! through BOTH doors and through the core directly. All three runs
//! must land on identical trial balances and identical entry streams
//! (modulo the door `source` tags on the entries this layer posts) —
//! and the configured invoice's COGS must come from the $91.00 line
//! snapshot on every run (exit test 3, the Day-3 trap through the
//! agent door).

use erpcraft_cli::commands::{dispatch, Door};
use erpcraft_core::rusqlite::Connection;
use erpcraft_core::{
    accept_quote, create_invoice_from_order, create_order_from_quote, create_quote, fulfil_order,
    get_entry, get_invoice, issue_invoice, list_entries, open_db, post_entry, record_payment,
    record_sales_return, send_quote, trial_balance, ConfigValue, JournalEntryView, NewEntry,
    NewLine, NewQuote, NewQuoteLine, ReturnLine, TrialBalance,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn steel_galvanized() -> BTreeMap<String, ConfigValue> {
    BTreeMap::from([
        (
            "material".to_string(),
            ConfigValue::Code("steel".to_string()),
        ),
        (
            "finish".to_string(),
            ConfigValue::Code("galvanized".to_string()),
        ),
    ])
}

/// The loop through one door. Returns (trial balance, entries).
fn door_run(door: Door) -> (TrialBalance, Vec<JournalEntryView>) {
    let mut conn = open_db(":memory:").unwrap();
    let d = |conn: &mut Connection, name: &str, args: Value| {
        dispatch(conn, door, name, &args)
            .unwrap_or_else(|e| panic!("{name} refused through {door:?}: {}", e.message))
    };

    let quote = d(
        &mut conn,
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
    // The configured line rolled up: $125.00 + $15.00 galvanized +
    // $22.00 reinforced (forced by the over-40 constraint) = $162.00,
    // cost $68.00 + $9.00 + $14.00 = $91.00.
    assert_eq!(quote["lines"][0]["unit_price_cents"], 16_200);
    assert_eq!(quote["lines"][0]["standard_cost_cents"], 9_100);
    let quote_id = quote["id"].as_i64().unwrap();

    d(&mut conn, "send_quote", json!({ "quote_id": quote_id }));
    d(&mut conn, "accept_quote", json!({ "quote_id": quote_id }));
    let order = d(&mut conn, "create_order", json!({ "quote_id": quote_id }));
    let order_id = order["id"].as_i64().unwrap();
    d(&mut conn, "fulfil_order", json!({ "order_id": order_id }));
    let invoice = d(&mut conn, "create_invoice", json!({ "order_id": order_id }));
    let invoice_id = invoice["id"].as_i64().unwrap();
    assert_eq!(
        invoice["status"], "draft",
        "the draft is visible before it posts"
    );

    let issued = d(
        &mut conn,
        "issue_invoice",
        json!({ "invoice_id": invoice_id }),
    );
    assert_eq!(issued["invoice"]["status"], "issued");
    assert_lines(
        &issued["revenue_entry"],
        &[("1100", 810_000, 0), ("4000", 0, 810_000)],
    );
    // Exit test 3: COGS is 50 × $91.00 from the snapshot — NOT
    // 50 × $68.00 = $340.00 from the item master.
    assert_lines(
        &issued["cogs_entry"],
        &[("5000", 455_000, 0), ("1220", 0, 455_000)],
    );

    let paid = d(
        &mut conn,
        "record_payment",
        json!({ "invoice_id": invoice_id, "amount_cents": 810_000 }),
    );
    assert_eq!(paid["payment"]["amount_cents"], 810_000);
    assert_lines(
        &paid["entry"],
        &[("1000", 810_000, 0), ("1100", 0, 810_000)],
    );

    let returned = d(
        &mut conn,
        "record_return",
        json!({ "invoice_id": invoice_id, "lines": [{ "item_id": 1, "qty": 2 }] }),
    );
    assert_lines(
        &returned["revenue_entry"],
        &[("4010", 32_400, 0), ("1100", 0, 32_400)],
    );
    assert_lines(
        &returned["cogs_entry"],
        &[("1220", 18_200, 0), ("5000", 0, 18_200)],
    );
    assert_eq!(
        returned["revenue_entry"]["reverses_entry_id"], issued["revenue_entry"]["id"],
        "the reversal points at the entry it corrects"
    );

    // One builder and one raw entry through the same door.
    let built = d(
        &mut conn,
        "buy_materials",
        json!({ "amount_cents": 360_000, "entry_date": "2026-10-29" }),
    );
    assert_eq!(built["memo"], "Buy materials");
    d(
        &mut conn,
        "post_entry",
        json!({
            "entry_date": "2026-10-29",
            "memo": "Supplies (parity)",
            "lines": [
                { "account_code": "6030", "debit_cents": 5_000 },
                { "account_code": "1000", "credit_cents": 5_000 }
            ]
        }),
    );

    (trial_balance(&conn).unwrap(), list_entries(&conn).unwrap())
}

/// The same loop through the core directly — the reference run.
fn core_run() -> (TrialBalance, Vec<JournalEntryView>) {
    let mut conn = open_db(":memory:").unwrap();
    let quote_id = create_quote(
        &mut conn,
        &NewQuote {
            customer_id: 1,
            lines: vec![NewQuoteLine {
                item_id: 1,
                qty: 50,
                configuration: Some(steel_galvanized()),
            }],
        },
    )
    .unwrap();
    send_quote(&mut conn, quote_id).unwrap();
    accept_quote(&mut conn, quote_id).unwrap();
    let order_id = create_order_from_quote(&mut conn, quote_id).unwrap();
    fulfil_order(&mut conn, order_id).unwrap();
    let invoice_id = create_invoice_from_order(&mut conn, order_id).unwrap();
    issue_invoice(&mut conn, invoice_id).unwrap();
    let invoice = get_invoice(&conn, invoice_id).unwrap().unwrap();
    let cogs = get_entry(&conn, invoice.cogs_entry_id.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        cogs.lines[0].debit_cents, 455_000,
        "core run: snapshot COGS"
    );
    record_payment(&mut conn, invoice_id, 810_000).unwrap();
    record_sales_return(&mut conn, invoice_id, &[ReturnLine { item_id: 1, qty: 2 }]).unwrap();
    post_entry(
        &mut conn,
        &NewEntry {
            entry_date: "2026-10-29".to_string(),
            memo: "Buy materials".to_string(),
            source: "manual".to_string(),
            reverses_entry_id: None,
            lines: vec![
                NewLine {
                    account_code: "1200".to_string(),
                    debit_cents: 360_000,
                    credit_cents: 0,
                },
                NewLine {
                    account_code: "2000".to_string(),
                    debit_cents: 0,
                    credit_cents: 360_000,
                },
            ],
        },
    )
    .unwrap();
    post_entry(
        &mut conn,
        &NewEntry {
            entry_date: "2026-10-29".to_string(),
            memo: "Supplies (parity)".to_string(),
            source: "manual".to_string(),
            reverses_entry_id: None,
            lines: vec![
                NewLine {
                    account_code: "6030".to_string(),
                    debit_cents: 5_000,
                    credit_cents: 0,
                },
                NewLine {
                    account_code: "1000".to_string(),
                    debit_cents: 0,
                    credit_cents: 5_000,
                },
            ],
        },
    )
    .unwrap();
    (trial_balance(&conn).unwrap(), list_entries(&conn).unwrap())
}

fn assert_lines(entry: &Value, expected: &[(&str, i64, i64)]) {
    let lines = entry["lines"].as_array().expect("entry has lines");
    assert_eq!(lines.len(), expected.len(), "line count for {entry}");
    for (line, (code, debit, credit)) in lines.iter().zip(expected.iter()) {
        assert_eq!(line["account_code"], *code);
        assert_eq!(line["debit_cents"], *debit, "{code} debit");
        assert_eq!(line["credit_cents"], *credit, "{code} credit");
    }
}

/// Entry-stream comparison with the door tags normalized away: the
/// engine sources (invoice:/payment:/return:) must match exactly; the
/// entries this layer posts carry cli / mcp / manual depending on the
/// run, and only the tag may differ.
fn normalized(entries: &[JournalEntryView]) -> Vec<Value> {
    entries
        .iter()
        .map(|e| {
            let source = if matches!(e.source.as_str(), "cli" | "mcp" | "manual") {
                "<door>".to_string()
            } else {
                e.source.clone()
            };
            json!({
                "entry_date": e.entry_date,
                "memo": e.memo,
                "source": source,
                "reverses_entry_id": e.reverses_entry_id,
                "status": e.status,
                "lines": e.lines.iter().map(|l| json!({
                    "account_code": l.account_code,
                    "debit_cents": l.debit_cents,
                    "credit_cents": l.credit_cents,
                })).collect::<Vec<_>>(),
            })
        })
        .collect()
}

#[test]
fn both_doors_and_the_core_land_identically() {
    let (cli_tb, cli_entries) = door_run(Door::Cli);
    let (mcp_tb, mcp_entries) = door_run(Door::Mcp);
    let (core_tb, core_entries) = core_run();

    // Trial balances identical across all three runs (exit test 1's
    // "ending trial balance matches the equivalent core-level run",
    // and exit test 5's parity of doors).
    assert_eq!(
        serde_json::to_value(&cli_tb).unwrap(),
        serde_json::to_value(&core_tb).unwrap(),
        "CLI door vs core"
    );
    assert_eq!(
        serde_json::to_value(&mcp_tb).unwrap(),
        serde_json::to_value(&core_tb).unwrap(),
        "MCP door vs core"
    );
    assert_eq!(cli_tb.total_debit_cents, cli_tb.total_credit_cents);

    // Entry streams identical modulo the door source tags.
    assert_eq!(normalized(&cli_entries), normalized(&core_entries));
    assert_eq!(normalized(&mcp_entries), normalized(&core_entries));

    // And the tags themselves are exactly what the prep demands:
    // builder/raw entries carry the door tag on each door.
    let cli_built = cli_entries
        .iter()
        .find(|e| e.memo == "Buy materials")
        .unwrap();
    assert_eq!(cli_built.source, "cli");
    let mcp_built = mcp_entries
        .iter()
        .find(|e| e.memo == "Buy materials")
        .unwrap();
    assert_eq!(mcp_built.source, "mcp");
}
