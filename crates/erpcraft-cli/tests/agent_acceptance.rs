//! The Day-5 acceptance suite: the agent contract spec's 12 test
//! prompts, run against the deterministic stub client (plus the
//! recorded client for test 12), each on a fresh seeded ledger with
//! the demo day pinned to Friday 2026-10-30.
//!
//! The gate is asserted everywhere it matters: a proposal — however
//! clear the prompt — posts nothing until `confirm` runs.

use erpcraft_cli::agent::clients::{FallbackClient, RecordedClient, StubClient, UnavailableClient};
use erpcraft_cli::agent::{
    Agent, AgentError, PriceChoice, Proposal, ProposeOptions, ProposeOutcome,
};
use erpcraft_cli::commands::{dispatch, Door};
use erpcraft_core::rusqlite::Connection;
use erpcraft_core::{entry_count, open_db};
use serde_json::{json, Value};

const TODAY: &str = "2026-10-30";
const T1: &str = "Invoice Acme for the 40 units we shipped Tuesday.";

fn db() -> Connection {
    open_db(":memory:").unwrap()
}

fn stub_agent() -> Agent<StubClient> {
    Agent::new(StubClient, TODAY)
}

fn entries(conn: &Connection) -> i64 {
    entry_count(conn).unwrap()
}

fn read(conn: &mut Connection, name: &str, args: Value) -> Value {
    dispatch(conn, Door::Cli, name, &args).unwrap()
}

fn proposal(outcome: ProposeOutcome) -> Proposal {
    match outcome {
        ProposeOutcome::NeedsConfirmation(p) => *p,
        other => panic!("expected a proposal, got {other:?}"),
    }
}

/// Run a prompt all the way through the gate; returns the confirm
/// result (the posted entries, read back).
fn post(agent: &Agent<StubClient>, conn: &mut Connection, prompt: &str) -> Value {
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

// -- Spec test 1 (+ the gate, + the prep's exit prompt) ---------------

#[test]
fn test_01_invoice_shipment_posts_only_after_confirm() {
    let mut conn = db();
    let agent = stub_agent();
    assert_eq!(entries(&conn), 1, "only the opening entry");

    let p = proposal(agent.propose(&mut conn, T1).unwrap());
    // THE GATE: the whole pre-gate chain ran (draft invoice exists),
    // but nothing posted.
    assert_eq!(entries(&conn), 1, "a proposal posts nothing");
    let invoices = read(&mut conn, "list_invoices", json!({}));
    assert_eq!(invoices.as_array().unwrap().len(), 1);
    assert_eq!(invoices[0]["status"], "draft");

    let text = &p.confirmation_text;
    assert!(
        text.contains("Invoice Acme for 40 brackets @ $125.00 = $5,000.00"),
        "{text}"
    );
    assert!(text.contains("Shipped Tuesday (2026-10-27)"), "{text}");
    assert!(text.contains("Accounts Receivable +$5,000.00"), "{text}");
    assert!(text.contains("Sales Revenue +$5,000.00"), "{text}");
    assert!(text.contains("Cost of Goods Sold +$2,720.00"), "{text}");
    assert!(
        text.contains("Finished Goods Inventory -$2,720.00"),
        "{text}"
    );
    assert!(
        text.contains("Total debits $7,720.00 = total credits $7,720.00 ✓"),
        "{text}"
    );

    let result = p.confirm(&mut conn).unwrap();
    assert_eq!(entries(&conn), 3);
    assert_eq!(result["invoice"]["status"], "issued");
    assert_eq!(line(&result["revenue_entry"], "1100"), (500_000, 0));
    assert_eq!(line(&result["revenue_entry"], "4000"), (0, 500_000));
    assert_eq!(line(&result["cogs_entry"], "5000"), (272_000, 0));
    assert_eq!(line(&result["cogs_entry"], "1220"), (0, 272_000));
}

#[test]
fn cancel_posts_nothing_and_the_draft_stays_unissued() {
    let mut conn = db();
    let agent = stub_agent();
    let p = proposal(agent.propose(&mut conn, T1).unwrap());
    assert!(p.cancel_message().contains("nothing posted"));
    assert_eq!(entries(&conn), 1);
    let invoices = read(&mut conn, "list_invoices", json!({}));
    assert_eq!(invoices[0]["status"], "draft", "cancel never issues");

    let rent = proposal(
        agent
            .propose(&mut conn, "Record this month's rent, $1,200.")
            .unwrap(),
    );
    drop(rent); // a builder proposal that is never confirmed
    assert_eq!(entries(&conn), 1, "an unconfirmed builder posts nothing");
}

// -- Spec test 2: payment ----------------------------------------------

#[test]
fn test_02_payment_against_an_invoice() {
    let mut conn = db();
    let agent = stub_agent();
    post(
        &agent,
        &mut conn,
        "Invoice Delta for the 40 units we shipped Tuesday.",
    );
    assert_eq!(entries(&conn), 3);

    let p = proposal(
        agent
            .propose(&mut conn, "Delta paid $3,125 against their invoice.")
            .unwrap(),
    );
    assert_eq!(entries(&conn), 3, "the gate holds for payments too");
    assert!(
        p.confirmation_text.contains("Cash +$3,125.00"),
        "{}",
        p.confirmation_text
    );
    let result = p.confirm(&mut conn).unwrap();
    assert_eq!(line(&result["entry"], "1000"), (312_500, 0));
    assert_eq!(line(&result["entry"], "1100"), (0, 312_500));
    let invoice = read(&mut conn, "get_invoice", json!({ "invoice_id": 1 }));
    assert_eq!(invoice["balance_cents"], 187_500);
    assert_eq!(invoice["status"], "partially_paid");
}

// -- Spec test 3: the deposit trap --------------------------------------

#[test]
fn test_03_deposit_is_a_liability_not_revenue() {
    let mut conn = db();
    let agent = stub_agent();
    let p = proposal(
        agent
            .propose(
                &mut conn,
                "Epsilon sent a $1,500 deposit for a future order.",
            )
            .unwrap(),
    );
    let text = &p.confirmation_text;
    assert!(text.contains("Customer Deposits"), "{text}");
    assert!(text.contains("a liability, not revenue"), "{text}");

    let entry = p.confirm(&mut conn).unwrap();
    assert_eq!(line(&entry, "1000"), (150_000, 0));
    assert_eq!(line(&entry, "2200"), (0, 150_000));
    // Rule 8: the posting carries who/what/why.
    assert_eq!(entry["source"], "agent");
    let memo = entry["memo"].as_str().unwrap();
    assert!(memo.contains("Record customer deposit (Epsilon)"), "{memo}");
    assert!(
        memo.contains("Epsilon sent a $1,500 deposit for a future order."),
        "the verbatim prompt rides in the memo: {memo}"
    );
    // Sales Revenue is untouched.
    let tb = read(&mut conn, "trial_balance", json!({}));
    let revenue = tb["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["code"] == "4000")
        .unwrap();
    assert_eq!(revenue["credit_cents"], 0);
}

// -- Spec test 4: return ------------------------------------------------

#[test]
fn test_04_return_references_the_original_sale() {
    let mut conn = db();
    let agent = stub_agent();
    let sale = post(
        &agent,
        &mut conn,
        "Invoice Delta for the 10 units we shipped Tuesday.",
    );
    let revenue_id = sale["invoice"]["gl_entry_id"].as_i64().unwrap();
    let cogs_id = sale["invoice"]["cogs_entry_id"].as_i64().unwrap();
    assert_eq!(entries(&conn), 3);

    let p = proposal(
        agent
            .propose(&mut conn, "Delta returned 2 defective brackets.")
            .unwrap(),
    );
    assert_eq!(entries(&conn), 3, "the gate holds for returns too");
    let result = p.confirm(&mut conn).unwrap();
    let revenue = &result["revenue_entry"];
    let cogs = &result["cogs_entry"];
    assert_eq!(line(revenue, "4010"), (25_000, 0));
    assert_eq!(line(revenue, "1100"), (0, 25_000));
    assert_eq!(
        revenue["reverses_entry_id"].as_i64().unwrap(),
        revenue_id,
        "the return's revenue entry points at the T8 issuance entry"
    );
    assert_eq!(line(cogs, "1220"), (13_600, 0));
    assert_eq!(line(cogs, "5000"), (0, 13_600));
    assert_eq!(cogs["reverses_entry_id"].as_i64().unwrap(), cogs_id);
}

// -- Spec tests 5 + 6: supplier payment, rent ----------------------------

#[test]
fn test_05_pay_the_steel_supplier() {
    let mut conn = db();
    let agent = stub_agent();
    let entry = post(
        &agent,
        &mut conn,
        "Pay $4,100 to the steel supplier against what we owe.",
    );
    assert_eq!(line(&entry, "2000"), (410_000, 0));
    assert_eq!(line(&entry, "1000"), (0, 410_000));
    assert_eq!(entry["source"], "agent");
}

#[test]
fn test_06_record_rent() {
    let mut conn = db();
    let agent = stub_agent();
    let entry = post(&agent, &mut conn, "Record this month's rent, $1,200.");
    assert_eq!(line(&entry, "6000"), (120_000, 0));
    assert_eq!(line(&entry, "1000"), (0, 120_000));
}

// -- Spec test 7: missing quantity ---------------------------------------

#[test]
fn test_07_missing_quantity_clarifies_and_posts_nothing() {
    let mut conn = db();
    let agent = stub_agent();
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
    let quotes = read(&mut conn, "list_quotes", json!({}));
    assert_eq!(
        quotes.as_array().unwrap().len(),
        0,
        "not even a draft was created"
    );
}

// -- Spec test 8: unknown customer (+ the as-built name trap) ------------

#[test]
fn test_08_unknown_customer_refuses_with_the_list() {
    let mut conn = db();
    let agent = stub_agent();
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
fn the_seeded_short_name_trap_clarifies_never_auto_matches() {
    let mut conn = db();
    let agent = stub_agent();
    // The spec's demo name was "Acme Manufacturing"; the seeded list
    // as built is the short form "Acme". Saying the long name must
    // clarify — an auto-match would silently post to a customer the
    // operator may not have meant.
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
    let quotes = read(&mut conn, "list_quotes", json!({}));
    assert_eq!(quotes.as_array().unwrap().len(), 0);
}

// -- Spec test 9: duplicate ----------------------------------------------

#[test]
fn test_09_duplicate_warns_and_needs_an_explicit_override() {
    let mut conn = db();
    let agent = stub_agent();
    post(&agent, &mut conn, T1);
    assert_eq!(entries(&conn), 3);

    match agent.propose(&mut conn, T1).unwrap() {
        ProposeOutcome::DuplicateWarning { warning, .. } => {
            assert!(warning.contains("duplicate"), "{warning}");
            assert!(warning.contains("invoice #1"), "{warning}");
        }
        other => panic!("expected a duplicate warning, got {other:?}"),
    }
    assert_eq!(entries(&conn), 3, "the warning itself posts nothing");
    let invoices = read(&mut conn, "list_invoices", json!({}));
    assert_eq!(
        invoices.as_array().unwrap().len(),
        1,
        "no second draft either"
    );

    // The explicit override prepares it — with the warning still
    // attached to the confirmation — and confirm posts the second.
    let options = ProposeOptions {
        allow_duplicate: true,
        ..Default::default()
    };
    let p = proposal(agent.propose_with(&mut conn, T1, &options).unwrap());
    assert!(
        p.duplicate_warning.is_some(),
        "the override keeps the warning visible"
    );
    p.confirm(&mut conn).unwrap();
    assert_eq!(entries(&conn), 5);
    let invoices = read(&mut conn, "list_invoices", json!({}));
    assert_eq!(invoices.as_array().unwrap().len(), 2);
}

// -- Spec test 10: production completion (the Day-5 builder) -------------

#[test]
fn test_10_production_completion() {
    let mut conn = db();
    let agent = stub_agent();
    let p = proposal(
        agent
            .propose(&mut conn, "We finished 50 brackets today, cost $4,100.")
            .unwrap(),
    );
    assert!(
        p.confirmation_text
            .contains("Finished Goods Inventory +$4,100.00"),
        "{}",
        p.confirmation_text
    );
    let entry = p.confirm(&mut conn).unwrap();
    assert_eq!(line(&entry, "1220"), (410_000, 0));
    assert_eq!(line(&entry, "1210"), (0, 410_000));
    assert_eq!(entry["source"], "agent");
}

// -- Spec test 11: price conflict -----------------------------------------

#[test]
fn test_11_price_conflict_flags_asks_and_posts_nothing() {
    let mut conn = db();
    let agent = stub_agent();
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
    assert_eq!(entries(&conn), 1);
    let quotes = read(&mut conn, "list_quotes", json!({}));
    assert_eq!(
        quotes.as_array().unwrap().len(),
        0,
        "nothing proposed, nothing created"
    );

    // Answering "the price-list price" proceeds at $125.00.
    let options = ProposeOptions {
        price_choice: Some(PriceChoice::ListPrice),
        ..Default::default()
    };
    let p = proposal(agent.propose_with(&mut conn, prompt, &options).unwrap());
    let result = p.confirm(&mut conn).unwrap();
    assert_eq!(line(&result["revenue_entry"], "1100"), (500_000, 0));

    // Answering "the one-off price" is refused honestly: the chain
    // snapshots the item master and no step can override it.
    let mut conn2 = db();
    let options = ProposeOptions {
        price_choice: Some(PriceChoice::OneOff),
        ..Default::default()
    };
    match agent.propose_with(&mut conn2, prompt, &options).unwrap() {
        ProposeOutcome::Refusal { message, .. } => {
            assert!(message.contains("one-off price"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(entries(&conn2), 1);
}

// -- Spec test 12: API down ------------------------------------------------

#[test]
fn test_12_api_down_falls_back_to_the_recording_then_fails_cleanly() {
    let fixture = include_str!("../fixtures/recorded_t1.json");
    let recorded = RecordedClient::from_json(fixture).unwrap();
    let client = FallbackClient::new(
        UnavailableClient {
            reason: "simulated outage".to_string(),
        },
        recorded,
    );
    let agent = Agent::new(client, TODAY);
    let mut conn = db();

    // The recorded exchange drives the same gated flow.
    let p = proposal(agent.propose(&mut conn, T1).unwrap());
    assert_eq!(entries(&conn), 1, "even the fallback respects the gate");
    p.confirm(&mut conn).unwrap();
    assert_eq!(entries(&conn), 3);

    // A prompt the recording doesn't cover: a clean unavailability
    // that names the CLI path — no crash, no silent post.
    let err = agent
        .propose(&mut conn, "Record this month's rent, $1,200.")
        .unwrap_err();
    match err {
        AgentError::ModelUnavailable(message) => {
            assert!(message.contains("erpcraft CLI"), "{message}");
        }
        other => panic!("expected ModelUnavailable, got {other:?}"),
    }
    assert_eq!(entries(&conn), 3);
}

// -- Out of scope -----------------------------------------------------------

#[test]
fn out_of_scope_prompts_are_refused_in_plain_language() {
    let mut conn = db();
    let agent = stub_agent();
    match agent
        .propose(&mut conn, "Create a purchase order for more steel.")
        .unwrap()
    {
        ProposeOutcome::OutOfScope { message, .. } => {
            assert!(
                message.contains("I don't handle purchase orders yet"),
                "{message}"
            );
            assert!(message.contains("I can invoice"), "{message}");
        }
        other => panic!("expected out of scope, got {other:?}"),
    }
    assert_eq!(entries(&conn), 1);
}
