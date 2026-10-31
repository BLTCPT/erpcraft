//! MRP at the doors: dispatch commands and the NL agent's planning
//! intents (MRP_DESIGN.md). The agent's check intent is a read-only
//! Answer; its release intent is a gated Proposal that creates a
//! document and posts no journal entry.

use erpcraft_cli::agent::clients::StubClient;
use erpcraft_cli::agent::{Agent, ProposeOutcome};
use erpcraft_cli::commands::{dispatch, Door};
use erpcraft_core::rusqlite::Connection;
use erpcraft_core::{entry_count, open_db, seed_mrp_demo_if_empty};
use serde_json::{json, Value};

fn db() -> Connection {
    let conn = open_db(":memory:").unwrap();
    seed_mrp_demo_if_empty(&conn).unwrap();
    conn
}

fn call(conn: &mut Connection, name: &str, args: Value) -> Value {
    dispatch(conn, Door::Cli, name, &args).unwrap()
}

#[test]
fn dispatch_mrp_surfaces_agree_with_the_scenario() {
    let mut conn = db();
    let plan = call(
        &mut conn,
        "run_mrp",
        json!({
            "as_of": "2026-10-31",
            "demand_sku": "BRK-100",
            "demand_qty": 200,
            "demand_date": "2026-11-15"
        }),
    );
    let orders = plan["planned_orders"].as_array().unwrap();
    assert_eq!(orders.len(), 2);
    assert_eq!(orders[0]["key"], "production:BRK-100:2026-11-15:150");
    assert_eq!(orders[1]["key"], "purchase:FST-SET:2026-11-10:60");
    assert!(plan["shortages"].as_array().unwrap().is_empty());

    // explain_shortage: FST-SET is planned, not short.
    let explained = call(
        &mut conn,
        "explain_shortage",
        json!({
            "sku": "FST-SET",
            "as_of": "2026-10-31",
            "demand_sku": "BRK-100",
            "demand_qty": 200,
            "demand_date": "2026-11-15"
        }),
    );
    assert_eq!(explained["shortage"], false);
    assert!(explained["explanation"]
        .as_str()
        .unwrap()
        .contains("planned, not short"));

    // Release through the dispatch door: document created, GL silent.
    let released = call(
        &mut conn,
        "release_planned_order",
        json!({
            "key": "purchase:FST-SET:2026-11-10:60",
            "as_of": "2026-10-31",
            "demand_sku": "BRK-100",
            "demand_qty": 200,
            "demand_date": "2026-11-15"
        }),
    );
    assert_eq!(released["kind"], "purchase");
    let pos = call(&mut conn, "list_purchase_orders", json!({}));
    assert_eq!(pos.as_array().unwrap().len(), 2, "seed PO + released PO");
    assert_eq!(entry_count(&conn).unwrap(), 1, "release posts nothing");
}

#[test]
fn agent_answers_can_we_build_from_a_live_run() {
    let mut conn = db();
    let agent = Agent::new(StubClient, "2026-10-31");
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
    assert_eq!(entry_count(&conn).unwrap(), 1, "a check posts nothing");
}

#[test]
fn agent_names_a_shortage_with_its_reasons() {
    let mut conn = db();
    let agent = Agent::new(StubClient, "2026-10-31");
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
    assert!(
        message.contains("BRK-100: production 150 due 2026-10-20"),
        "{message}"
    );
}

#[test]
fn agent_release_is_gated_and_posts_no_journal_entry() {
    let mut conn = db();
    let agent = Agent::new(StubClient, "2026-10-31");
    let outcome = agent
        .propose(
            &mut conn,
            "Release purchase:FST-SET:2026-11-10:60 from the 200 BRK-100 by 2026-11-15 plan.",
        )
        .unwrap();
    let ProposeOutcome::NeedsConfirmation(proposal) = outcome else {
        panic!("expected a proposal, got {outcome:?}");
    };
    assert!(
        proposal
            .confirmation_text
            .contains("Release planned purchase order — FST-SET ×60"),
        "{}",
        proposal.confirmation_text
    );
    assert!(
        proposal
            .confirmation_text
            .contains("No journal entry posts"),
        "{}",
        proposal.confirmation_text
    );
    // The gate: nothing exists until confirm.
    let pos = call(&mut conn, "list_purchase_orders", json!({}));
    assert_eq!(
        pos.as_array().unwrap().len(),
        1,
        "only the seed PO before confirm"
    );
    let result = proposal.confirm(&mut conn).unwrap();
    assert_eq!(result["kind"], "purchase");
    let pos = call(&mut conn, "list_purchase_orders", json!({}));
    assert_eq!(pos.as_array().unwrap().len(), 2);
    assert_eq!(
        entry_count(&conn).unwrap(),
        1,
        "release posts no journal entry"
    );
}

#[test]
fn agent_release_of_an_unknown_key_is_refused() {
    let mut conn = db();
    let agent = Agent::new(StubClient, "2026-10-31");
    let outcome = agent
        .propose(&mut conn, "Release purchase:FST-SET:2026-11-10:999 please.")
        .unwrap();
    let ProposeOutcome::Refusal { message, .. } = outcome else {
        panic!("expected a refusal, got {outcome:?}");
    };
    assert!(message.contains("No planned order with key"), "{message}");
}
