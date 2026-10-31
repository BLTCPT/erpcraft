//! Live-eval request capture (LIVE_EVAL.md): for every evaluation
//! scenario, build the ledger state exactly as the acceptance
//! suites do, run the real adapter with a capturing client, and
//! write the exact (system, user) messages the live route would
//! send — so the recorded live replies are answers to the genuine
//! production parse requests, not reconstructions.
//!
//! Usage: cargo run -p erpcraft-cli --example live_eval -- <out.json>
//! The capturing client answers with the stub parse so `propose`
//! runs its course; the proposal is discarded — only the captured
//! request is kept. Scenario state is throwaway (in-memory DBs).

use erpcraft_cli::agent::clients::{render_parse_messages, stub_parse, ModelClient, StubClient};
use erpcraft_cli::agent::{Agent, AgentError, ParseRequest};
use erpcraft_cli::commands::{dispatch, Door};
use erpcraft_core::rusqlite::Connection;
use erpcraft_core::{open_db, seed_mrp_demo_if_empty, seed_procurement_if_empty};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Clone)]
struct CapturingClient {
    captured: Rc<RefCell<Option<ParseRequest>>>,
}

impl ModelClient for CapturingClient {
    fn parse_intent(&self, request: &ParseRequest) -> Result<String, AgentError> {
        *self.captured.borrow_mut() = Some(request.clone());
        Ok(stub_parse(&request.prompt, &request.today, &request.suppliers).to_string())
    }
}

fn d(conn: &mut Connection, name: &str, args: Value) -> Value {
    dispatch(conn, Door::Cli, name, &args).unwrap()
}

/// Post a setup prompt through the stub agent (state building only —
/// the scenario's own prompt is what gets captured).
fn stub_post(conn: &mut Connection, prompt: &str) {
    let agent = Agent::new(StubClient, "2026-10-30");
    match agent.propose(conn, prompt).unwrap() {
        erpcraft_cli::agent::ProposeOutcome::NeedsConfirmation(p) => {
            p.confirm(conn).unwrap();
        }
        other => panic!("setup prompt did not propose: {other:?}"),
    }
}

fn fresh() -> Connection {
    open_db(":memory:").unwrap()
}

fn mrp_db() -> Connection {
    let conn = fresh();
    seed_mrp_demo_if_empty(&conn).unwrap();
    conn
}

/// P2P state: Harbor Components PO for 10 × BRK-100 @ $60.00;
/// `stage` 0 = PO only, 1 = received, 2 = matched AP invoice,
/// 3 = on-hold AP invoice (price variance).
fn p2p_db(stage: i64) -> Connection {
    let mut conn = fresh();
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
    conn
}

type Setup = Box<dyn Fn() -> Connection>;
type Scenario = (&'static str, &'static str, Setup, &'static str);

fn main() {
    let out_path = std::env::args()
        .nth(1)
        .expect("usage: live_eval <requests.json>");
    let t1 = "Invoice Acme for the 40 units we shipped Tuesday.";
    let scenarios: Vec<Scenario> = vec![
        ("t1_invoice", "2026-10-30", Box::new(fresh), t1),
        (
            "t2_payment",
            "2026-10-30",
            Box::new(|| {
                let mut conn = fresh();
                stub_post(
                    &mut conn,
                    "Invoice Delta for the 40 units we shipped Tuesday.",
                );
                conn
            }),
            "Delta paid $3,125 against their invoice.",
        ),
        (
            "t3_deposit",
            "2026-10-30",
            Box::new(fresh),
            "Epsilon sent a $1,500 deposit for a future order.",
        ),
        (
            "t4_return",
            "2026-10-30",
            Box::new(|| {
                let mut conn = fresh();
                stub_post(
                    &mut conn,
                    "Invoice Delta for the 10 units we shipped Tuesday.",
                );
                conn
            }),
            "Delta returned 2 defective brackets.",
        ),
        (
            "t5_pay_supplier",
            "2026-10-30",
            Box::new(fresh),
            "Pay $4,100 to the steel supplier against what we owe.",
        ),
        (
            "t6_rent",
            "2026-10-30",
            Box::new(fresh),
            "Record this month's rent, $1,200.",
        ),
        (
            "t7_missing_qty",
            "2026-10-30",
            Box::new(fresh),
            "Invoice Acme for the units we shipped.",
        ),
        (
            "t8_unknown_customer",
            "2026-10-30",
            Box::new(fresh),
            "Invoice Globex for 10 brackets.",
        ),
        (
            "t8b_name_trap",
            "2026-10-30",
            Box::new(fresh),
            "Invoice Acme Manufacturing for 10 brackets.",
        ),
        (
            "t9_duplicate",
            "2026-10-30",
            Box::new(|| {
                let mut conn = fresh();
                stub_post(&mut conn, t1);
                conn
            }),
            t1,
        ),
        (
            "t10_production",
            "2026-10-30",
            Box::new(fresh),
            "We finished 50 brackets today, cost $4,100.",
        ),
        (
            "t11_price_conflict",
            "2026-10-30",
            Box::new(fresh),
            "Invoice Acme for 40 units at $130 each.",
        ),
        (
            "oos_purchase_order",
            "2026-10-30",
            Box::new(fresh),
            "Create a purchase order for more steel.",
        ),
        (
            "mrp_check_build",
            "2026-10-31",
            Box::new(mrp_db),
            "Can we build 200 BRK-100 by Nov 15?",
        ),
        (
            "mrp_check_shortage",
            "2026-10-31",
            Box::new(mrp_db),
            "Can we build 200 BRK-100 by 2026-10-20?",
        ),
        (
            "mrp_check_plain",
            "2026-10-31",
            Box::new(mrp_db),
            "What are we short on?",
        ),
        (
            "mrp_release",
            "2026-10-31",
            Box::new(mrp_db),
            "Release purchase:FST-SET:2026-11-10:60 from the 200 BRK-100 by 2026-11-15 plan.",
        ),
        (
            "mrp_release_unknown",
            "2026-10-31",
            Box::new(mrp_db),
            "Release purchase:FST-SET:2026-11-10:999 please.",
        ),
        (
            "mrp_config_refusal",
            "2026-10-31",
            Box::new(mrp_db),
            "Can we build 200 BRK-100 in aluminum with galvanized finish by 2026-11-15?",
        ),
        (
            "p2p_receive",
            "2026-10-30",
            Box::new(|| p2p_db(0)),
            "Receive 10 BRK-100 against purchase order 1.",
        ),
        (
            "p2p_pay",
            "2026-10-30",
            Box::new(|| p2p_db(2)),
            "Pay AP invoice 1.",
        ),
        (
            "p2p_held_refusal",
            "2026-10-30",
            Box::new(|| p2p_db(3)),
            "Pay AP invoice 1.",
        ),
        (
            "p2p_held_override",
            "2026-10-30",
            Box::new(|| p2p_db(3)),
            "Pay AP invoice 1 anyway — the price increase was approved by Dana.",
        ),
    ];

    let mut out = Vec::new();
    for (id, today, setup, prompt) in &scenarios {
        let mut conn = setup();
        let cell: Rc<RefCell<Option<ParseRequest>>> = Rc::new(RefCell::new(None));
        let agent = Agent::new(
            CapturingClient {
                captured: cell.clone(),
            },
            *today,
        );
        let _ = agent.propose(&mut conn, prompt);
        let request = cell
            .borrow()
            .clone()
            .unwrap_or_else(|| panic!("scenario {id}: the adapter never called the client"));
        let (system, user) = render_parse_messages(&request);
        out.push(json!({
            "id": id,
            "prompt": prompt,
            "today": today,
            "system": system,
            "user": user,
        }));
        println!("captured {id}");
    }
    std::fs::write(&out_path, serde_json::to_string_pretty(&out).unwrap()).unwrap();
    println!("wrote {} scenarios to {out_path}", out.len());
}
