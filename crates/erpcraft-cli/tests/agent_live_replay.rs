//! Live-eval replay (LIVE_EVAL.md): the acceptance scenarios'
//! prompts, parsed by the LIVE Baseten model (recorded in
//! `fixtures/recorded_live_baseten.json` on 2026-10-31 through the
//! connector route), replayed through the real adapter via the
//! RecordedClient — the same propose → confirm gate the stub suite
//! exercises. Every assertion mirrors the stub acceptance suite:
//! identical postings, refusals, and balances are the contract;
//! the model's wording quirks are reported in LIVE_EVAL.md, never
//! asserted away here.
//!
//! Setup state is built with the StubClient/dispatch (deterministic
//! state building); only the scenario's own prompt is live-parsed.

use erpcraft_cli::agent::clients::{RecordedClient, StubClient};
use erpcraft_cli::agent::{Agent, PriceChoice, Proposal, ProposeOptions, ProposeOutcome};
use erpcraft_cli::commands::{dispatch, Door};
use erpcraft_core::rusqlite::Connection;
use erpcraft_core::{entry_count, open_db, seed_mrp_demo_if_empty, seed_procurement_if_empty};
use serde_json::{json, Value};

const TODAY: &str = "2026-10-30";
const MRP_TODAY: &str = "2026-10-31";
const T1: &str = "Invoice Acme for the 40 units we shipped Tuesday.";

fn live_agent(today: &str) -> Agent<RecordedClient> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/recorded_live_baseten.json"
    );
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("live fixture missing at {path}: {e}"));
    let client = RecordedClient::from_json(&text).unwrap();
    Agent::new(client, today)
}

fn db() -> Connection {
    open_db(":memory:").unwrap()
}

fn mrp_db() -> Connection {
    let conn = db();
    seed_mrp_demo_if_empty(&conn).unwrap();
    conn
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

fn stub_post(conn: &mut Connection, prompt: &str) -> Value {
    let agent = Agent::new(StubClient, TODAY);
    let p = proposal(agent.propose(conn, prompt).unwrap());
    p.confirm(conn).unwrap()
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

// -- The twelve contract prompts, live-parsed --------------------------

#[test]
fn live_t1_invoice_posts_only_after_confirm() {
    let mut conn = db();
    let agent = live_agent(TODAY);
    let p = proposal(agent.propose(&mut conn, T1).unwrap());
    assert_eq!(entries(&conn), 1, "a proposal posts nothing");
    assert!(
        p.confirmation_text
            .contains("Total debits $7,720.00 = total credits $7,720.00 ✓"),
        "{}",
        p.confirmation_text
    );
    let result = p.confirm(&mut conn).unwrap();
    assert_eq!(entries(&conn), 3);
    assert_eq!(line(&result["revenue_entry"], "1100"), (500_000, 0));
    assert_eq!(line(&result["revenue_entry"], "4000"), (0, 500_000));
    assert_eq!(line(&result["cogs_entry"], "5000"), (272_000, 0));
    assert_eq!(line(&result["cogs_entry"], "1220"), (0, 272_000));
}

#[test]
fn live_t2_payment_against_an_invoice() {
    let mut conn = db();
    stub_post(
        &mut conn,
        "Invoice Delta for the 40 units we shipped Tuesday.",
    );
    let agent = live_agent(TODAY);
    let p = proposal(
        agent
            .propose(&mut conn, "Delta paid $3,125 against their invoice.")
            .unwrap(),
    );
    assert!(
        p.confirmation_text.contains("Cash +$3,125.00"),
        "{}",
        p.confirmation_text
    );
    let result = p.confirm(&mut conn).unwrap();
    assert_eq!(line(&result["entry"], "1000"), (312_500, 0));
    assert_eq!(line(&result["entry"], "1100"), (0, 312_500));
    let invoice = d(&mut conn, "get_invoice", json!({ "invoice_id": 1 }));
    assert_eq!(invoice["balance_cents"], 187_500);
    assert_eq!(invoice["status"], "partially_paid");
}

#[test]
fn live_t3_deposit_is_a_liability_not_revenue() {
    let mut conn = db();
    let agent = live_agent(TODAY);
    let p = proposal(
        agent
            .propose(
                &mut conn,
                "Epsilon sent a $1,500 deposit for a future order.",
            )
            .unwrap(),
    );
    assert!(
        p.confirmation_text.contains("a liability, not revenue"),
        "{}",
        p.confirmation_text
    );
    let entry = p.confirm(&mut conn).unwrap();
    assert_eq!(line(&entry, "1000"), (150_000, 0));
    assert_eq!(line(&entry, "2200"), (0, 150_000));
    assert_eq!(entry["source"], "agent");
}

#[test]
fn live_t4_return_references_the_original_sale() {
    let mut conn = db();
    let sale = stub_post(
        &mut conn,
        "Invoice Delta for the 10 units we shipped Tuesday.",
    );
    let revenue_id = sale["invoice"]["gl_entry_id"].as_i64().unwrap();
    let cogs_id = sale["invoice"]["cogs_entry_id"].as_i64().unwrap();
    let agent = live_agent(TODAY);
    let p = proposal(
        agent
            .propose(&mut conn, "Delta returned 2 defective brackets.")
            .unwrap(),
    );
    let result = p.confirm(&mut conn).unwrap();
    assert_eq!(line(&result["revenue_entry"], "4010"), (25_000, 0));
    assert_eq!(
        result["revenue_entry"]["reverses_entry_id"]
            .as_i64()
            .unwrap(),
        revenue_id
    );
    assert_eq!(line(&result["cogs_entry"], "1220"), (13_600, 0));
    assert_eq!(
        result["cogs_entry"]["reverses_entry_id"].as_i64().unwrap(),
        cogs_id
    );
}

#[test]
fn live_t5_pay_the_steel_supplier() {
    let mut conn = db();
    let agent = live_agent(TODAY);
    let p = proposal(
        agent
            .propose(
                &mut conn,
                "Pay $4,100 to the steel supplier against what we owe.",
            )
            .unwrap(),
    );
    let entry = p.confirm(&mut conn).unwrap();
    assert_eq!(line(&entry, "2000"), (410_000, 0));
    assert_eq!(line(&entry, "1000"), (0, 410_000));
}

#[test]
fn live_t6_record_rent() {
    let mut conn = db();
    let agent = live_agent(TODAY);
    let p = proposal(
        agent
            .propose(&mut conn, "Record this month's rent, $1,200.")
            .unwrap(),
    );
    let entry = p.confirm(&mut conn).unwrap();
    assert_eq!(line(&entry, "6000"), (120_000, 0));
    assert_eq!(line(&entry, "1000"), (0, 120_000));
}

#[test]
fn live_t7_missing_quantity_clarifies_and_posts_nothing() {
    let mut conn = db();
    let agent = live_agent(TODAY);
    match agent
        .propose(&mut conn, "Invoice Acme for the units we shipped.")
        .unwrap()
    {
        ProposeOutcome::Clarification { question, .. } => {
            assert_eq!(question, "How many units, and which day?");
        }
        other => panic!("expected a clarification, got {other:?}"),
    }
    assert_eq!(entries(&conn), 1);
}

#[test]
fn live_t8_unknown_customer_refuses_with_the_list() {
    let mut conn = db();
    let agent = live_agent(TODAY);
    match agent
        .propose(&mut conn, "Invoice Globex for 10 brackets.")
        .unwrap()
    {
        ProposeOutcome::Refusal { message, .. } => {
            assert!(
                message.contains("Globex isn't in the customer list"),
                "{message}"
            );
            assert!(message.contains("Acme, Beta, Delta, Epsilon"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(entries(&conn), 1);
}

#[test]
fn live_t8b_seeded_name_trap_clarifies_never_auto_matches() {
    let mut conn = db();
    let agent = live_agent(TODAY);
    match agent
        .propose(&mut conn, "Invoice Acme Manufacturing for 10 brackets.")
        .unwrap()
    {
        ProposeOutcome::Clarification { question, .. } => {
            assert!(question.contains("Did you mean Acme?"), "{question}");
        }
        other => panic!("expected a clarification, got {other:?}"),
    }
    assert_eq!(entries(&conn), 1);
}

#[test]
fn live_t9_duplicate_warns() {
    let mut conn = db();
    stub_post(&mut conn, T1);
    let agent = live_agent(TODAY);
    match agent.propose(&mut conn, T1).unwrap() {
        ProposeOutcome::DuplicateWarning { warning, .. } => {
            assert!(warning.contains("invoice #1"), "{warning}");
        }
        other => panic!("expected a duplicate warning, got {other:?}"),
    }
    assert_eq!(entries(&conn), 3);
}

#[test]
fn live_t10_production_completion() {
    let mut conn = db();
    let agent = live_agent(TODAY);
    let p = proposal(
        agent
            .propose(&mut conn, "We finished 50 brackets today, cost $4,100.")
            .unwrap(),
    );
    let entry = p.confirm(&mut conn).unwrap();
    assert_eq!(line(&entry, "1220"), (410_000, 0));
    assert_eq!(line(&entry, "1210"), (0, 410_000));
}

#[test]
fn live_t11_price_conflict_flags_then_posts_at_list_price() {
    let mut conn = db();
    let agent = live_agent(TODAY);
    let prompt = "Invoice Acme for 40 units at $130 each.";
    match agent.propose(&mut conn, prompt).unwrap() {
        ProposeOutcome::PriceConflict { question, .. } => {
            assert!(
                question.contains("The price list says $125.00/unit"),
                "{question}"
            );
            assert!(question.contains("$130.00"), "{question}");
        }
        other => panic!("expected a price conflict, got {other:?}"),
    }
    let options = ProposeOptions {
        price_choice: Some(PriceChoice::ListPrice),
        ..Default::default()
    };
    let p = proposal(agent.propose_with(&mut conn, prompt, &options).unwrap());
    let result = p.confirm(&mut conn).unwrap();
    assert_eq!(line(&result["revenue_entry"], "1100"), (500_000, 0));
}

#[test]
fn live_out_of_scope_is_refused_in_plain_language() {
    let mut conn = db();
    let agent = live_agent(TODAY);
    match agent
        .propose(&mut conn, "Create a purchase order for more steel.")
        .unwrap()
    {
        ProposeOutcome::OutOfScope { message, .. } => {
            // The stub's topic is "purchase orders"; the live model
            // answered "create purchase order" (LIVE_EVAL.md) — the
            // outcome and the can-do list are the contract.
            assert!(message.contains("I don't handle"), "{message}");
            assert!(message.contains("I can invoice"), "{message}");
        }
        other => panic!("expected out of scope, got {other:?}"),
    }
    assert_eq!(entries(&conn), 1);
}

// -- MRP intents, live-parsed -------------------------------------------

#[test]
fn live_mrp_check_answers_from_a_live_run() {
    let mut conn = mrp_db();
    let agent = live_agent(MRP_TODAY);
    let outcome = agent
        .propose(&mut conn, "Can we build 200 BRK-100 by Nov 15?")
        .unwrap();
    let ProposeOutcome::Answer { message, .. } = outcome else {
        panic!("expected an Answer, got {outcome:?}");
    };
    assert!(
        message.contains("BRK-100: demand 200 by 2026-11-15"),
        "{message}"
    );
    assert!(
        message.contains("builds 150 more, starting 2026-11-10"),
        "{message}"
    );
    assert!(message.contains("Shortages: none"), "{message}");
    assert_eq!(entries(&conn), 1);
}

#[test]
fn live_mrp_check_names_a_shortage() {
    let mut conn = mrp_db();
    let agent = live_agent(MRP_TODAY);
    let outcome = agent
        .propose(&mut conn, "Can we build 200 BRK-100 by 2026-10-20?")
        .unwrap();
    let ProposeOutcome::Answer { message, .. } = outcome else {
        panic!("expected an Answer, got {outcome:?}");
    };
    assert!(
        message.contains("already 16 day(s) past — this is a shortage"),
        "{message}"
    );
}

#[test]
fn live_mrp_check_plain_shortage_question() {
    let mut conn = mrp_db();
    let agent = live_agent(MRP_TODAY);
    let outcome = agent.propose(&mut conn, "What are we short on?").unwrap();
    let ProposeOutcome::Answer { message, .. } = outcome else {
        panic!("expected an Answer, got {outcome:?}");
    };
    assert!(message.contains("MRP run as of 2026-10-31"), "{message}");
    assert_eq!(entries(&conn), 1);
}

#[test]
fn live_mrp_release_is_gated_and_posts_no_journal_entry() {
    let mut conn = mrp_db();
    let agent = live_agent(MRP_TODAY);
    let p = proposal(
        agent
            .propose(
                &mut conn,
                "Release purchase:FST-SET:2026-11-10:60 from the 200 BRK-100 by 2026-11-15 plan.",
            )
            .unwrap(),
    );
    assert!(
        p.confirmation_text
            .contains("Release planned purchase order — FST-SET ×60"),
        "{}",
        p.confirmation_text
    );
    let result = p.confirm(&mut conn).unwrap();
    assert_eq!(result["kind"], "purchase");
    assert_eq!(entries(&conn), 1, "release posts no journal entry");
}

#[test]
fn live_mrp_release_of_an_unknown_key_is_refused() {
    let mut conn = mrp_db();
    let agent = live_agent(MRP_TODAY);
    match agent
        .propose(&mut conn, "Release purchase:FST-SET:2026-11-10:999 please.")
        .unwrap()
    {
        ProposeOutcome::Refusal { message, .. } => {
            assert!(message.contains("No planned order with key"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn live_mrp_configured_what_if_surfaces_the_configurator_refusal() {
    let mut conn = mrp_db();
    let agent = live_agent(MRP_TODAY);
    // The model extracted the configuration; the refusal text is
    // the deterministic core evaluator's explanation — rendered
    // from core data, never from model prose.
    match agent
        .propose(
            &mut conn,
            "Can we build 200 BRK-100 in aluminum with galvanized finish by 2026-11-15?",
        )
        .unwrap()
    {
        ProposeOutcome::Refusal { message, .. } => {
            assert!(
                message.contains("what-if configuration for BRK-100 refused"),
                "{message}"
            );
            assert!(
                message.contains("Galvanized finish is available only on steel brackets"),
                "{message}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(entries(&conn), 1);
}

// -- P2P intents, live-parsed --------------------------------------------

/// P2P state mirroring tests/p2p_agent.rs: stage 0 = PO open,
/// 2 = received + matched AP invoice, 3 = received + held invoice.
fn p2p_db(stage: i64) -> (Connection, i64, i64) {
    let mut conn = db();
    seed_procurement_if_empty(&conn).unwrap();
    let supplier = d(
        &mut conn,
        "create_supplier",
        json!({ "name": "Harbor Components" }),
    );
    let order = d(
        &mut conn,
        "create_purchase_order",
        json!({
            "supplier_id": supplier["id"],
            "lines": [{ "item_id": 1, "qty": 10, "unit_price_cents": 6000 }]
        }),
    );
    let po_id = order["id"].as_i64().unwrap();
    let po_line_id = order["lines"][0]["id"].as_i64().unwrap();
    if stage >= 1 {
        d(
            &mut conn,
            "receive_goods",
            json!({ "order_id": po_id, "lines": [{ "po_line_id": po_line_id, "qty": 10 }] }),
        );
    }
    if stage >= 2 {
        let price = if stage >= 3 { 6500 } else { 6000 };
        d(
            &mut conn,
            "record_ap_invoice",
            json!({
                "order_id": po_id,
                "lines": [{ "po_line_id": po_line_id, "qty": 10, "unit_price_cents": price }]
            }),
        );
    }
    (conn, po_id, po_line_id)
}

#[test]
fn live_receive_goods_is_gated_and_posts_the_grir_entry() {
    let (mut conn, po_id, _) = p2p_db(0);
    let agent = live_agent(TODAY);
    let p = proposal(
        agent
            .propose(&mut conn, "Receive 10 BRK-100 against purchase order 1.")
            .unwrap(),
    );
    assert_eq!(entries(&conn), 1, "a proposal posts nothing");
    let result = p.confirm(&mut conn).unwrap();
    assert_eq!(line(&result["entry"], "1200"), (60_000, 0));
    assert_eq!(line(&result["entry"], "2050"), (0, 60_000));
    let order = d(
        &mut conn,
        "get_procurement_order",
        json!({ "order_id": po_id }),
    );
    assert_eq!(order["status"], "received");
}

#[test]
fn live_pay_ap_invoice_posts_after_confirm() {
    let (mut conn, _, _) = p2p_db(2);
    let agent = live_agent(TODAY);
    let p = proposal(agent.propose(&mut conn, "Pay AP invoice 1.").unwrap());
    assert_eq!(entries(&conn), 3, "the gate holds");
    let result = p.confirm(&mut conn).unwrap();
    assert_eq!(result["payment"]["amount_cents"], 60_000);
    let invoice = d(&mut conn, "get_ap_invoice", json!({ "ap_invoice_id": 1 }));
    assert_eq!(invoice["status"], "paid");
}

#[test]
fn live_held_invoice_refuses_with_its_variances() {
    let (mut conn, _, _) = p2p_db(3);
    let agent = live_agent(TODAY);
    // Same prompt as the matched case, different state: the live
    // parse is the same pay_ap_invoice; the adapter's hold refusal
    // is what fires. (Sweep 2 recorded "unclear" here — the prompt
    // fix in LIVE_EVAL.md is what makes this the precise refusal.)
    match agent.propose(&mut conn, "Pay AP invoice 1.").unwrap() {
        ProposeOutcome::Refusal { message, .. } => {
            assert!(message.contains("on three-way-match hold"), "{message}");
            assert!(message.contains("invoiced at 6500 cents"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn live_held_invoice_override_then_payment() {
    let (mut conn, _, _) = p2p_db(3);
    let agent = live_agent(TODAY);
    let p = proposal(
        agent
            .propose(
                &mut conn,
                "Pay AP invoice 1 anyway — the price increase was approved by Dana.",
            )
            .unwrap(),
    );
    let text = &p.confirmation_text;
    assert!(text.contains("ON THREE-WAY-MATCH HOLD"), "{text}");
    assert!(text.contains("overrider Dana"), "{text}");
    let before = entries(&conn);
    let result = p.confirm(&mut conn).unwrap();
    assert_eq!(entries(&conn), before + 1, "only the payment posts");
    let invoice = d(&mut conn, "get_ap_invoice", json!({ "ap_invoice_id": 1 }));
    assert_eq!(invoice["status"], "paid");
    assert_eq!(invoice["match_status"], "overridden");
    assert_eq!(invoice["overridden_by"], "Dana");
    // The live model kept the prompt's full justification clause,
    // attribution included (LIVE_EVAL.md) — the contract is that the
    // stated justification is what lands on the invoice.
    let reason = invoice["override_reason"].as_str().unwrap();
    assert!(reason.contains("price increase"), "{reason}");
    assert_eq!(
        result["pay_ap_invoice"]["payment"]["amount_cents"], 65_000,
        "the payment step ran after the override: {result}"
    );
}
