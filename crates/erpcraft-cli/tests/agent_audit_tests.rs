//! Audit regressions for the agent layer (external review, Oct 31).
//! Each test mirrors a reviewer probe that asserted the DEFECTIVE
//! behavior; these assert the fixed behavior instead.

use erpcraft_cli::agent::clients::{ModelClient, StubClient};
use erpcraft_cli::agent::{Agent, AgentError, ParseRequest, ProposeOutcome};
use erpcraft_cli::commands::{dispatch, Door};
use erpcraft_core::rusqlite::Connection;
use erpcraft_core::*;
use serde_json::json;
use std::collections::BTreeMap;

/// A model that invents override attribution the human never wrote.
struct FabricatedOverride;
impl ModelClient for FabricatedOverride {
    fn parse_intent(
        &self,
        _: &ParseRequest,
    ) -> std::result::Result<String, AgentError> {
        Ok(json!({
            "intent": "pay_ap_invoice",
            "ap_invoice_id": 1,
            "override_reason": "price increase authorized by Dana",
            "overrider": "Dana"
        })
        .to_string())
    }
}

/// An AP invoice on three-way-match hold (invoiced $65 vs PO $60).
fn held() -> Connection {
    let mut c = open_db(":memory:").unwrap();
    seed_procurement_if_empty(&c).unwrap();
    let s = list_suppliers(&c).unwrap()[0].id;
    let p = create_purchase_order(
        &mut c,
        &NewProcurementOrder {
            supplier_id: Some(s),
            requisition_id: None,
            lines: vec![ProcLineInput {
                item_id: 1,
                qty: 10,
                unit_price_cents: 6000,
            }],
        },
    )
    .unwrap();
    let l = get_procurement_order(&c, p).unwrap().unwrap().lines[0].id;
    receive_goods(
        &mut c,
        p,
        &[ReceiveLineInput {
            po_line_id: l,
            qty: 10,
        }],
    )
    .unwrap();
    record_ap_invoice(
        &mut c,
        &NewApInvoice {
            po_id: p,
            lines: vec![ApLineInput {
                po_line_id: l,
                qty: 10,
                unit_price_cents: 6500,
            }],
            supplier_ref: None,
        },
    )
    .unwrap();
    c
}

#[test]
fn fabricated_override_attribution_is_refused() {
    let mut c = held();
    // The human's prompt contains neither the reason nor the name;
    // the model's invented attribution must not become an override.
    let outcome = Agent::new(FabricatedOverride, "2026-10-31")
        .propose(&mut c, "Pay AP invoice 1.")
        .unwrap();
    assert!(
        matches!(outcome, ProposeOutcome::Refusal { .. }),
        "expected a refusal, got {outcome:?}"
    );
    let invoice = get_ap_invoice(&c, 1).unwrap().unwrap();
    assert_eq!(invoice.status, "on_hold");
    assert_eq!(invoice.overridden_by, None);
    assert!(list_ap_payments(&c).unwrap().is_empty());
}

#[test]
fn failed_sequence_leaves_no_override_behind() {
    let mut c = held();
    let period = create_fiscal_period(&c, "closed year", "2026-01-01", "2026-12-31").unwrap();
    close_fiscal_period(&c, period).unwrap();
    // The human DID write the reason and the name — the override is
    // legitimately proposed — but the payment leg cannot post into a
    // closed period, so the whole action must refuse BEFORE the
    // override step runs.
    let outcome = Agent::new(FabricatedOverride, "2026-10-31")
        .propose(
            &mut c,
            "Pay AP invoice 1. Override the match: price increase authorized by Dana, overrider Dana.",
        )
        .unwrap();
    let ProposeOutcome::NeedsConfirmation(p) = outcome else {
        panic!("expected a proposal, got {outcome:?}");
    };
    assert!(p.confirm(&mut c).is_err());
    let invoice = get_ap_invoice(&c, 1).unwrap().unwrap();
    assert_eq!(invoice.match_status, "variance", "no override may survive");
    assert_eq!(invoice.overridden_by, None);
    assert_eq!(invoice.status, "on_hold");
    assert!(list_ap_payments(&c).unwrap().is_empty());
}

#[test]
fn supplier_payment_above_threshold_needs_an_approver_everywhere() {
    let mut c = open_db(":memory:").unwrap();
    set_approval_threshold(&c, 100).unwrap(); // $1.00
    // The agent refuses up front...
    let outcome = Agent::new(StubClient, "2026-10-31")
        .propose(&mut c, "Pay the supplier $4,100.")
        .unwrap();
    assert!(
        matches!(outcome, ProposeOutcome::Refusal { .. }),
        "expected a refusal, got {outcome:?}"
    );
    // ...and the dispatch gate refuses direct callers too...
    let refused = dispatch(
        &mut c,
        Door::Cli,
        "pay_supplier",
        &json!({ "amount_cents": 410000 }),
    );
    assert!(refused.is_err());
    assert_eq!(entry_count(&c).unwrap(), 1);
    // ...while a properly attributed payment posts, approver in memo.
    create_user(&c, "Sam", "clerk").unwrap();
    create_user(&c, "Dana", "approver").unwrap();
    let posted = dispatch(
        &mut c,
        Door::Cli,
        "pay_supplier",
        &json!({ "amount_cents": 410000, "actor": "Sam", "approver": "Dana" }),
    )
    .unwrap();
    assert!(posted.to_string().contains("approved by Dana"), "{posted}");
    assert_eq!(entry_count(&c).unwrap(), 2);
}

#[test]
fn confirming_the_same_action_twice_posts_once() {
    let mut c = open_db(":memory:").unwrap();
    let agent = Agent::new(StubClient, "2026-10-31");
    let outcome = agent
        .propose(&mut c, "Pay the supplier $4,100.")
        .unwrap();
    let ProposeOutcome::NeedsConfirmation(p) = outcome else {
        panic!("expected a proposal, got {outcome:?}");
    };
    p.confirm(&mut c).unwrap();
    assert_eq!(entry_count(&c).unwrap(), 2);
    // A replay of the same confirmation is refused, not re-posted.
    assert!(p.confirm(&mut c).is_err());
    assert_eq!(entry_count(&c).unwrap(), 2);

    // Two separately built proposals for the identical shipment:
    // the first confirms, the second is a replay.
    let mut c2 = open_db(":memory:").unwrap();
    let a = Agent::new(StubClient, "2026-10-31");
    let get = |o: ProposeOutcome| match o {
        ProposeOutcome::NeedsConfirmation(p) => p,
        other => panic!("expected a proposal, got {other:?}"),
    };
    let p1 = get(a
        .propose(&mut c2, "Invoice Acme for the 40 units we shipped Tuesday.")
        .unwrap());
    let p2 = get(a
        .propose(&mut c2, "Invoice Acme for the 40 units we shipped Tuesday.")
        .unwrap());
    p1.confirm(&mut c2).unwrap();
    assert!(p2.confirm(&mut c2).is_err());
    // Both proposals created draft invoices (draft state is not a
    // posting), but exactly one may ever issue.
    let issued = list_invoices(&c2)
        .unwrap()
        .into_iter()
        .filter(|i| i.status == "issued")
        .count();
    assert_eq!(issued, 1);
    assert_eq!(entry_count(&c2).unwrap(), 3); // opening + revenue + COGS
}

#[test]
fn equal_total_different_quantity_is_a_clarification_not_an_invoice() {
    let mut c = open_db(":memory:").unwrap();
    // A fulfilled order for 125 CONFIGURED brackets at $162 totals
    // $20,250 — exactly what 162 plain brackets at $125 total.
    let configuration = BTreeMap::from([
        ("material".to_string(), ConfigValue::Code("steel".to_string())),
        ("finish".to_string(), ConfigValue::Code("galvanized".to_string())),
    ]);
    let q = create_quote(
        &mut c,
        &NewQuote {
            customer_id: 1,
            lines: vec![NewQuoteLine {
                item_id: 1,
                qty: 125,
                configuration: Some(configuration),
            }],
        },
    )
    .unwrap();
    send_quote(&mut c, q).unwrap();
    accept_quote(&mut c, q).unwrap();
    let o = create_order_from_quote(&mut c, q).unwrap();
    fulfil_order(&mut c, o).unwrap();

    let outcome = Agent::new(StubClient, "2026-10-31")
        .propose(&mut c, "Invoice Acme for the 162 units we shipped Tuesday.")
        .unwrap();
    match outcome {
        ProposeOutcome::Clarification { question, .. } => {
            assert!(question.contains("125"), "the facts are named: {question}");
        }
        other => panic!("expected a clarification, got {other:?}"),
    }
    // Nothing was invoiced against the wrong order.
    assert_eq!(invoice_count(&c).unwrap(), 0);
}
