//! The procure-to-pay agent intents (ENTERPRISE_GAPS.md §3 at the
//! agent door): receiving against a purchase order and paying a
//! supplier (AP) invoice, both behind the propose → confirm gate.
//!
//! The three-way-match hold is the sharp edge: an on-hold invoice
//! cannot be proposed for payment at all unless the prompt itself
//! carries the override reason and the overrider's name — and then
//! the proposal shows the hold, the variances, and the override
//! step before the payment step.

use erpcraft_cli::agent::clients::{RecordedClient, StubClient};
use erpcraft_cli::agent::{Agent, Proposal, ProposeOutcome};
use erpcraft_cli::commands::{dispatch, Door};
use erpcraft_core::rusqlite::Connection;
use erpcraft_core::{entry_count, open_db};
use serde_json::{json, Value};

const TODAY: &str = "2026-10-30";
const SUPPLIER: &str = "Harbor Components";

fn db() -> Connection {
    open_db(":memory:").unwrap()
}

fn stub_agent() -> Agent<StubClient> {
    Agent::new(StubClient, TODAY)
}

fn entries(conn: &Connection) -> i64 {
    entry_count(conn).unwrap()
}

fn d(conn: &mut Connection, name: &str, args: Value) -> Value {
    dispatch(conn, Door::Cli, name, &args).unwrap()
}

fn proposal(outcome: ProposeOutcome) -> Proposal {
    match outcome {
        ProposeOutcome::NeedsConfirmation(p) => *p,
        other => panic!("expected a proposal, got {other:?}"),
    }
}

/// A purchase order for 10 × BRK-100 at $60.00 from Harbor
/// Components; returns (po_id, po_line_id).
fn make_po(conn: &mut Connection) -> (i64, i64) {
    let supplier = d(conn, "create_supplier", json!({ "name": SUPPLIER }));
    let supplier_id = supplier["id"].as_i64().unwrap();
    let order = d(
        conn,
        "create_purchase_order",
        json!({
            "supplier_id": supplier_id,
            "lines": [{ "item_id": 1, "qty": 10, "unit_price_cents": 6000 }]
        }),
    );
    (
        order["id"].as_i64().unwrap(),
        order["lines"][0]["id"].as_i64().unwrap(),
    )
}

fn receive_all(conn: &mut Connection, po_id: i64, po_line_id: i64) {
    d(
        conn,
        "receive_goods",
        json!({
            "order_id": po_id,
            "lines": [{ "po_line_id": po_line_id, "qty": 10 }]
        }),
    );
}

/// Record an AP invoice at the given unit price; returns its id.
fn record_invoice(conn: &mut Connection, po_id: i64, po_line_id: i64, price: i64) -> i64 {
    let invoiced = d(
        conn,
        "record_ap_invoice",
        json!({
            "order_id": po_id,
            "lines": [{ "po_line_id": po_line_id, "qty": 10, "unit_price_cents": price }]
        }),
    );
    invoiced["ap_invoice"]["id"].as_i64().unwrap()
}

fn line(entry: &Value, code: &str) -> (i64, i64) {
    let l = entry["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["account_code"] == code)
        .unwrap_or_else(|| panic!("entry has no {code} line: {entry}"));
    (
        l["debit_cents"].as_i64().unwrap(),
        l["credit_cents"].as_i64().unwrap(),
    )
}

fn stock_on_hand(conn: &mut Connection, item_id: i64) -> i64 {
    let stock = d(conn, "list_stock", json!({}));
    stock
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["item_id"] == item_id)
        .map(|s| s["on_hand"].as_i64().unwrap())
        .unwrap_or(0)
}

// -- Receiving ---------------------------------------------------------

#[test]
fn receive_goods_is_gated_and_posts_the_grir_entry() {
    let mut conn = db();
    let (po_id, _) = make_po(&mut conn);
    let agent = stub_agent();
    assert_eq!(entries(&conn), 1, "only the opening entry");

    let p = proposal(
        agent
            .propose(&mut conn, "Receive 10 BRK-100 against purchase order 1.")
            .unwrap(),
    );
    // THE GATE: nothing received, nothing posted, until confirm.
    assert_eq!(entries(&conn), 1, "a proposal posts nothing");
    assert_eq!(
        stock_on_hand(&mut conn, 1),
        0,
        "a proposal receives nothing"
    );
    let text = &p.confirmation_text;
    assert!(
        text.contains("Receive BRK-100 ×10 against purchase order #1 — Harbor Components"),
        "{text}"
    );
    assert!(text.contains("Raw Materials Inventory +$600.00"), "{text}");
    assert!(
        text.contains("Goods Received / Invoice Received (GR-IR) +$600.00"),
        "{text}"
    );
    assert!(
        text.contains("Total debits $600.00 = total credits $600.00 ✓"),
        "{text}"
    );

    let result = p.confirm(&mut conn).unwrap();
    assert_eq!(entries(&conn), 2);
    assert_eq!(line(&result["entry"], "1200"), (60_000, 0));
    assert_eq!(line(&result["entry"], "2050"), (0, 60_000));
    assert_eq!(stock_on_hand(&mut conn, 1), 10);
    let order = d(
        &mut conn,
        "get_procurement_order",
        json!({ "order_id": po_id }),
    );
    assert_eq!(order["status"], "received");
}

#[test]
fn receive_with_no_quantity_completes_the_order() {
    let mut conn = db();
    make_po(&mut conn);
    let agent = stub_agent();
    let p = proposal(
        agent
            .propose(&mut conn, "Receive the Harbor Components delivery.")
            .unwrap(),
    );
    assert!(
        p.confirmation_text.contains("BRK-100 ×10"),
        "{}",
        p.confirmation_text
    );
    p.confirm(&mut conn).unwrap();
    assert_eq!(stock_on_hand(&mut conn, 1), 10);
}

#[test]
fn receive_more_than_ordered_is_refused_at_proposal() {
    let mut conn = db();
    make_po(&mut conn);
    let agent = stub_agent();
    match agent
        .propose(&mut conn, "Receive 12 BRK-100 against purchase order 1.")
        .unwrap()
    {
        ProposeOutcome::Refusal { message, .. } => {
            assert!(message.contains("Only 10 of BRK-100 remain"), "{message}");
            assert!(message.contains("over-receipt is not allowed"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(entries(&conn), 1);
    assert_eq!(stock_on_hand(&mut conn, 1), 0);
}

#[test]
fn receive_against_an_unknown_or_closed_order_is_refused() {
    let mut conn = db();
    let (po_id, po_line_id) = make_po(&mut conn);
    let agent = stub_agent();
    match agent
        .propose(&mut conn, "Receive 5 BRK-100 against purchase order 9.")
        .unwrap()
    {
        ProposeOutcome::Refusal { message, .. } => {
            assert!(message.contains("no purchase order #9"), "{message}");
            assert!(message.contains("Open for receiving: #1"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    receive_all(&mut conn, po_id, po_line_id);
    match agent
        .propose(&mut conn, "Receive 5 BRK-100 against purchase order 1.")
        .unwrap()
    {
        ProposeOutcome::Refusal { message, .. } => {
            assert!(
                message.contains("nothing more can be received"),
                "{message}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// -- Paying a matched invoice -------------------------------------------

#[test]
fn pay_ap_invoice_is_gated_and_posts_after_confirm() {
    let mut conn = db();
    let (po_id, po_line_id) = make_po(&mut conn);
    receive_all(&mut conn, po_id, po_line_id);
    let ap_id = record_invoice(&mut conn, po_id, po_line_id, 6000);
    assert_eq!(entries(&conn), 3, "opening + receipt + AP invoice");
    let agent = stub_agent();

    let p = proposal(agent.propose(&mut conn, "Pay AP invoice 1.").unwrap());
    assert_eq!(entries(&conn), 3, "the gate holds for AP payments too");
    let text = &p.confirmation_text;
    assert!(
        text.contains("Pay Harbor Components $600.00 against AP invoice #1"),
        "{text}"
    );
    assert!(text.contains("Accounts Payable -$600.00"), "{text}");
    assert!(text.contains("Cash -$600.00"), "{text}");

    let result = p.confirm(&mut conn).unwrap();
    assert_eq!(entries(&conn), 4);
    // A single-call gate returns the dispatch result bare (the same
    // contract as record_payment); the sequence gate keys by step.
    assert_eq!(result["payment"]["amount_cents"], 60_000);
    assert_eq!(line(&result["entry"], "2000"), (60_000, 0));
    assert_eq!(line(&result["entry"], "1000"), (0, 60_000));
    let invoice = d(
        &mut conn,
        "get_ap_invoice",
        json!({ "ap_invoice_id": ap_id }),
    );
    assert_eq!(invoice["status"], "paid");
}

#[test]
fn pay_ap_invoice_partial_then_overpay_is_refused() {
    let mut conn = db();
    let (po_id, po_line_id) = make_po(&mut conn);
    receive_all(&mut conn, po_id, po_line_id);
    record_invoice(&mut conn, po_id, po_line_id, 6000);
    let agent = stub_agent();

    let p = proposal(
        agent
            .propose(&mut conn, "Pay $200 toward AP invoice 1.")
            .unwrap(),
    );
    p.confirm(&mut conn).unwrap();
    let invoice = d(&mut conn, "get_ap_invoice", json!({ "ap_invoice_id": 1 }));
    assert_eq!(invoice["status"], "partially_paid");
    assert_eq!(invoice["balance_cents"], 40_000);

    match agent
        .propose(&mut conn, "Pay $500 toward AP invoice 1.")
        .unwrap()
    {
        ProposeOutcome::Refusal { message, .. } => {
            assert!(
                message.contains("more than the $400.00 balance"),
                "{message}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn pay_by_supplier_name_resolves_and_ambiguity_clarifies() {
    let mut conn = db();
    let (po_id, po_line_id) = make_po(&mut conn);
    receive_all(&mut conn, po_id, po_line_id);
    record_invoice(&mut conn, po_id, po_line_id, 6000);
    let agent = stub_agent();

    // One open invoice for the named supplier: it resolves.
    let p = proposal(
        agent
            .propose(&mut conn, "Pay the Harbor Components AP invoice.")
            .unwrap(),
    );
    assert!(
        p.confirmation_text.contains("AP invoice #1"),
        "{}",
        p.confirmation_text
    );
    drop(p);

    // A second open invoice for the same supplier: now it must ask.
    let supplier = d(&mut conn, "list_suppliers", json!({}));
    let supplier_id = supplier[0]["id"].as_i64().unwrap();
    let order2 = d(
        &mut conn,
        "create_purchase_order",
        json!({
            "supplier_id": supplier_id,
            "lines": [{ "item_id": 1, "qty": 5, "unit_price_cents": 6000 }]
        }),
    );
    let po2 = order2["id"].as_i64().unwrap();
    let line2 = order2["lines"][0]["id"].as_i64().unwrap();
    d(
        &mut conn,
        "receive_goods",
        json!({ "order_id": po2, "lines": [{ "po_line_id": line2, "qty": 5 }] }),
    );
    d(
        &mut conn,
        "record_ap_invoice",
        json!({
            "order_id": po2,
            "lines": [{ "po_line_id": line2, "qty": 5, "unit_price_cents": 6000 }]
        }),
    );
    match agent
        .propose(&mut conn, "Pay the Harbor Components AP invoice.")
        .unwrap()
    {
        ProposeOutcome::Clarification { question, .. } => {
            assert!(question.contains("2 open AP invoices"), "{question}");
            assert!(question.contains("#1"), "{question}");
            assert!(question.contains("#2"), "{question}");
        }
        other => panic!("expected a clarification, got {other:?}"),
    }
}

#[test]
fn pay_an_unknown_supplier_is_refused_with_the_list() {
    let mut conn = db();
    let (po_id, po_line_id) = make_po(&mut conn);
    receive_all(&mut conn, po_id, po_line_id);
    record_invoice(&mut conn, po_id, po_line_id, 6000);
    // The parse names a supplier that is not on file (the shape a
    // live model returns for this prompt; the adapter refuses).
    let recorded = RecordedClient::from_json(
        r#"{"exchanges":[{"prompt":"Pay the Globex Supply invoice.","response":{"intent":"pay_ap_invoice","supplier":"Globex Supply"}}]}"#,
    )
    .unwrap();
    let agent = Agent::new(recorded, TODAY);
    match agent
        .propose(&mut conn, "Pay the Globex Supply invoice.")
        .unwrap()
    {
        ProposeOutcome::Refusal { message, .. } => {
            assert!(
                message.contains("Globex Supply isn't in the supplier list"),
                "{message}"
            );
            assert!(message.contains("Harbor Components"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// -- The three-way-match hold -------------------------------------------

fn held_invoice(conn: &mut Connection) -> i64 {
    let (po_id, po_line_id) = make_po(conn);
    receive_all(conn, po_id, po_line_id);
    // $5.00/unit over the PO price: a price variance → on hold.
    let ap_id = record_invoice(conn, po_id, po_line_id, 6500);
    let invoice = d(conn, "get_ap_invoice", json!({ "ap_invoice_id": ap_id }));
    assert_eq!(invoice["status"], "on_hold");
    ap_id
}

#[test]
fn held_invoice_cannot_be_proposed_for_payment_without_a_reason() {
    let mut conn = db();
    held_invoice(&mut conn);
    let agent = stub_agent();
    let before = entries(&conn);
    match agent.propose(&mut conn, "Pay AP invoice 1.").unwrap() {
        ProposeOutcome::Refusal { message, .. } => {
            assert!(message.contains("on three-way-match hold"), "{message}");
            assert!(message.contains("invoiced at 6500 cents"), "{message}");
            assert!(
                message.contains("can't be proposed for payment"),
                "{message}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(entries(&conn), before);
}

#[test]
fn held_invoice_override_reason_without_a_name_clarifies() {
    let mut conn = db();
    held_invoice(&mut conn);
    let agent = stub_agent();
    match agent
        .propose(
            &mut conn,
            "Pay AP invoice 1 anyway — the price increase was approved.",
        )
        .unwrap()
    {
        ProposeOutcome::Clarification { question, .. } => {
            assert!(
                question.contains("Who is overriding the match?"),
                "{question}"
            );
        }
        other => panic!("expected a clarification, got {other:?}"),
    }
}

#[test]
fn held_invoice_with_reason_and_name_proposes_override_then_payment() {
    let mut conn = db();
    let ap_id = held_invoice(&mut conn);
    let agent = stub_agent();
    let before = entries(&conn);

    let p = proposal(
        agent
            .propose(
                &mut conn,
                "Pay AP invoice 1 anyway — the price increase was approved by Dana.",
            )
            .unwrap(),
    );
    // The gate: the hold still stands and nothing posted.
    assert_eq!(entries(&conn), before, "a proposal posts nothing");
    let invoice = d(
        &mut conn,
        "get_ap_invoice",
        json!({ "ap_invoice_id": ap_id }),
    );
    assert_eq!(invoice["status"], "on_hold", "a proposal overrides nothing");

    // The hold surfaces in the rendering, with the override step
    // (reason + name, recorded) ahead of the payment step.
    let text = &p.confirmation_text;
    assert!(text.contains("ON THREE-WAY-MATCH HOLD"), "{text}");
    assert!(text.contains("invoiced at 6500 cents"), "{text}");
    assert!(text.contains("Step 1 — override the match"), "{text}");
    assert!(text.contains("reason \"the price increase\""), "{text}");
    assert!(text.contains("overrider Dana"), "{text}");
    assert!(
        text.contains("Step 2 — pay Harbor Components $650.00"),
        "{text}"
    );

    let result = p.confirm(&mut conn).unwrap();
    assert_eq!(
        entries(&conn),
        before + 1,
        "only the payment posts an entry"
    );
    let invoice = d(
        &mut conn,
        "get_ap_invoice",
        json!({ "ap_invoice_id": ap_id }),
    );
    assert_eq!(invoice["status"], "paid");
    assert_eq!(invoice["match_status"], "overridden");
    assert_eq!(invoice["override_reason"], "the price increase");
    assert_eq!(invoice["overridden_by"], "Dana");
    assert_eq!(
        result["pay_ap_invoice"]["payment"]["amount_cents"], 65_000,
        "the payment step ran after the override: {result}"
    );
}
