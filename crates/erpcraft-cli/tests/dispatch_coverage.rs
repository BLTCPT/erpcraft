//! Dispatch coverage: every command in the table is exercised at
//! least once — the reads and lifecycle transitions the scenario
//! tests don't touch (decline/expire/cancel, the get_/list_ reads,
//! account_ledger), plus a full configurator authoring round trip
//! through the stretch commands (build a model, evaluate it, see the
//! authored dependency fire). AGENTS.md: tests accompany every
//! command.

use erpcraft_cli::commands::{dispatch, Door};
use erpcraft_core::open_db;
use erpcraft_core::rusqlite::Connection;
use serde_json::{json, Value};

fn d(conn: &mut Connection, name: &str, args: Value) -> Value {
    dispatch(conn, Door::Cli, name, &args)
        .unwrap_or_else(|e| panic!("{name} refused: {}", e.message))
}

#[test]
fn every_read_and_transition_dispatches() {
    let mut conn = open_db(":memory:").unwrap();

    // Reads over the seeded opening state.
    let ledger = d(&mut conn, "account_ledger", json!({ "code": "1000" }));
    assert_eq!(ledger["code"], "1000");
    assert_eq!(
        ledger["lines"].as_array().unwrap().len(),
        1,
        "the opening entry"
    );
    assert_eq!(
        ledger["balance_cents"], 4_820_000,
        "opening cash $48,200.00"
    );
    let opening = d(&mut conn, "get_entry", json!({ "entry_id": 1 }));
    assert_eq!(opening["source"], "opening");
    assert_eq!(
        d(&mut conn, "list_entries", json!({}))
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        d(&mut conn, "list_customers", json!({}))
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        d(&mut conn, "list_items", json!({}))
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // A quote that gets declined, one that expires, one that flows on.
    let declined = d(
        &mut conn,
        "create_quote",
        json!({ "customer_id": 2, "lines": [{ "item_id": 1, "qty": 1 }] }),
    );
    let dq = declined["id"].clone();
    d(&mut conn, "send_quote", json!({ "quote_id": dq }));
    let q = d(&mut conn, "decline_quote", json!({ "quote_id": dq }));
    assert_eq!(q["status"], "declined");
    assert_eq!(
        d(&mut conn, "get_quote", json!({ "quote_id": dq }))["status"],
        "declined"
    );

    let expiring = d(
        &mut conn,
        "create_quote",
        json!({ "customer_id": 2, "lines": [{ "item_id": 1, "qty": 1 }] }),
    );
    let eq = expiring["id"].clone();
    d(&mut conn, "send_quote", json!({ "quote_id": eq }));
    let q = d(&mut conn, "expire_quote", json!({ "quote_id": eq }));
    assert_eq!(q["status"], "expired");
    assert_eq!(
        d(&mut conn, "list_quotes", json!({}))
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // An order that gets cancelled before fulfilment.
    let quote = d(
        &mut conn,
        "create_quote",
        json!({ "customer_id": 3, "lines": [{ "item_id": 1, "qty": 5 }] }),
    );
    let qid = quote["id"].clone();
    d(&mut conn, "send_quote", json!({ "quote_id": qid }));
    d(&mut conn, "accept_quote", json!({ "quote_id": qid }));
    let order = d(&mut conn, "create_order", json!({ "quote_id": qid }));
    let oid = order["id"].clone();
    let o = d(&mut conn, "cancel_order", json!({ "order_id": oid }));
    assert_eq!(o["status"], "cancelled");
    assert_eq!(
        d(&mut conn, "get_order", json!({ "order_id": oid }))["status"],
        "cancelled"
    );
    assert_eq!(
        d(&mut conn, "list_orders", json!({}))
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Invoice reads over an empty book.
    assert_eq!(
        d(&mut conn, "list_invoices", json!({}))
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        d(&mut conn, "list_payments", json!({}))
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        d(
            &mut conn,
            "list_payments_for_invoice",
            json!({ "invoice_id": 1 })
        )
        .as_array()
        .unwrap()
        .len(),
        0
    );
    let missing = dispatch(
        &mut conn,
        Door::Cli,
        "get_invoice",
        &json!({ "invoice_id": 99 }),
    );
    assert!(
        missing.is_err(),
        "a missing invoice is a refusal, not a panic"
    );
}

#[test]
fn configurator_authoring_round_trip() {
    let mut conn = open_db(":memory:").unwrap();

    // One config model per item, and the only seeded item already has
    // the demo model — so the round trip authors a model for a
    // fixture item inserted directly (the item master is seed-only on
    // every surface; there is no create_item command by design).
    conn.execute(
        "INSERT INTO items (sku, name, unit_price_cents, standard_cost_cents)
         VALUES ('TST-100', 'Coverage widget', 1000, 400)",
        [],
    )
    .unwrap();

    let model = d(
        &mut conn,
        "create_config_model",
        json!({ "item_id": 2, "name": "Coverage model" }),
    );
    let model_id = model["id"].as_i64().unwrap();
    assert_eq!(model_id, 2, "the seeded BRK-100 model is id 1");

    d(
        &mut conn,
        "add_characteristic",
        json!({
            "model_id": model_id,
            "code": "size",
            "name": "Size",
            "value_type": "enum",
            "values": [
                { "code": "S", "label": "Small" },
                { "code": "L", "label": "Large" }
            ]
        }),
    );
    d(
        &mut conn,
        "create_component",
        json!({
            "model_id": model_id,
            "code": "LARGE-PLATE",
            "name": "Large plate",
            "price_delta_cents": 500,
            "cost_delta_cents": 200
        }),
    );
    d(
        &mut conn,
        "create_dependency",
        json!({
            "model_id": model_id,
            "kind": "selection",
            "target": "component:LARGE-PLATE",
            "expr": { "type": "eq", "char": "size", "value": "L" },
            "source_text": "Large size includes the large plate."
        }),
    );

    let fetched = d(
        &mut conn,
        "get_config_model",
        json!({ "model_id": model_id }),
    );
    assert_eq!(fetched["characteristics"].as_array().unwrap().len(), 1);
    assert_eq!(fetched["components"].as_array().unwrap().len(), 1);
    assert_eq!(fetched["dependencies"].as_array().unwrap().len(), 1);
    assert_eq!(
        d(&mut conn, "list_config_models", json!({}))
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // The authored selection dependency fires for size L...
    let evaluation = d(
        &mut conn,
        "evaluate_config",
        json!({ "model_id": model_id, "configuration": { "size": "L" } }),
    );
    assert_eq!(evaluation["verdict"]["status"], "consistent");
    assert_eq!(evaluation["selected_components"], json!(["LARGE-PLATE"]));

    // ...and not for size S.
    let evaluation = d(
        &mut conn,
        "evaluate_config",
        json!({ "model_id": model_id, "configuration": { "size": "S" } }),
    );
    assert_eq!(evaluation["verdict"]["status"], "consistent");
    assert_eq!(evaluation["selected_components"], json!([]));
}
