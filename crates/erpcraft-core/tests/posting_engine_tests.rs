//! Day-3 posting-engine tests for the ERPCraft core.
//!
//! The contract (DAY3_PREP.md): issuance and payments post balanced GL
//! entries in the same transaction as the document transition; the
//! double-post guard lives on the document headers; COGS reads the
//! invoice lines' standard-cost *snapshots* (the reconciliation
//! addendum rule); refused transitions post nothing; sales returns are
//! reversing entries, never edits.

use erpcraft_core::rusqlite::Connection;
use erpcraft_core::{
    accept_quote, account_ledger, create_invoice_from_order, create_order_from_quote, create_quote,
    entry_count, get_entry, get_invoice, issue_invoice, list_customers, list_items,
    list_payments_for_invoice, open_db, record_payment, record_sales_return, send_quote,
    ConfigValue, LedgerError, NewQuote, NewQuoteLine, ReturnLine, BRACKET_SKU,
    BRACKET_UNIT_PRICE_CENTS, STANDARD_COST_CENTS,
};
use std::collections::BTreeMap;

fn customer_id(conn: &Connection, name: &str) -> i64 {
    list_customers(conn)
        .unwrap()
        .into_iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("customer {name} is seeded"))
        .id
}

fn bracket_id(conn: &Connection) -> i64 {
    list_items(conn)
        .unwrap()
        .into_iter()
        .find(|i| i.sku == BRACKET_SKU)
        .expect("bracket item is seeded")
        .id
}

fn code(value: &str) -> ConfigValue {
    ConfigValue::Code(value.to_string())
}

fn config(pairs: &[(&str, ConfigValue)]) -> BTreeMap<String, ConfigValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

/// Drive a plain quote to a draft invoice; returns (order_id, invoice_id).
fn draft_invoice(conn: &mut Connection, customer: &str, qty: i64) -> (i64, i64) {
    let new_quote = NewQuote {
        customer_id: customer_id(conn, customer),
        lines: vec![NewQuoteLine {
            item_id: bracket_id(conn),
            qty,
            configuration: None,
        }],
    };
    let quote_id = create_quote(conn, &new_quote).unwrap();
    send_quote(conn, quote_id).unwrap();
    accept_quote(conn, quote_id).unwrap();
    let order_id = create_order_from_quote(conn, quote_id).unwrap();
    let invoice_id = create_invoice_from_order(conn, order_id).unwrap();
    (order_id, invoice_id)
}

fn issued_invoice(conn: &mut Connection, customer: &str, qty: i64) -> i64 {
    let (_, invoice_id) = draft_invoice(conn, customer, qty);
    issue_invoice(conn, invoice_id).unwrap();
    invoice_id
}

// ---------------------------------------------------------------------------
// Double-post guard: issuing twice posts once.
// ---------------------------------------------------------------------------

#[test]
fn issuing_twice_posts_exactly_once() {
    let mut conn = open_db(":memory:").unwrap();
    let invoice_id = issued_invoice(&mut conn, "Acme", 10);
    let invoice = get_invoice(&conn, invoice_id).unwrap().unwrap();
    let gl = invoice.gl_entry_id.expect("revenue entry recorded");
    let cogs = invoice.cogs_entry_id.expect("COGS entry recorded");
    assert_eq!(entry_count(&conn).unwrap(), 3); // opening + revenue + COGS

    // The second attempt is a clean refusal — no duplicate entries, and
    // the recorded entry ids are untouched.
    let err = issue_invoice(&mut conn, invoice_id).unwrap_err();
    assert!(err.to_string().contains("double-post"), "got: {err}");
    assert_eq!(entry_count(&conn).unwrap(), 3);
    let invoice = get_invoice(&conn, invoice_id).unwrap().unwrap();
    assert_eq!(invoice.gl_entry_id, Some(gl));
    assert_eq!(invoice.cogs_entry_id, Some(cogs));
    assert_eq!(invoice.status, "issued");
}

// ---------------------------------------------------------------------------
// Atomicity: a posting failure leaves the invoice in draft, with no
// partial entry and no guard ids set. The failure is the addendum's
// missing-standard-cost case: a line whose snapshot is NULL.
// ---------------------------------------------------------------------------

#[test]
fn missing_line_standard_cost_refuses_issuance_atomically() {
    let mut conn = open_db(":memory:").unwrap();
    let (_, invoice_id) = draft_invoice(&mut conn, "Beta", 5);
    // Simulate a legacy line with no cost snapshot.
    conn.execute(
        "UPDATE invoice_lines SET standard_cost_cents = NULL WHERE invoice_id = ?1",
        [invoice_id],
    )
    .unwrap();
    let entries_before = entry_count(&conn).unwrap();

    let err = issue_invoice(&mut conn, invoice_id).unwrap_err();
    assert!(
        err.to_string().contains("standard-cost snapshot"),
        "got: {err}"
    );
    let invoice = get_invoice(&conn, invoice_id).unwrap().unwrap();
    assert_eq!(invoice.status, "draft", "the transition rolled back");
    assert_eq!(invoice.gl_entry_id, None);
    assert_eq!(invoice.cogs_entry_id, None);
    assert_eq!(
        entry_count(&conn).unwrap(),
        entries_before,
        "no partial entry persists"
    );
}

// ---------------------------------------------------------------------------
// Refused payments post nothing.
// ---------------------------------------------------------------------------

#[test]
fn payment_on_draft_invoice_is_rejected_and_posts_nothing() {
    let mut conn = open_db(":memory:").unwrap();
    let (_, invoice_id) = draft_invoice(&mut conn, "Acme", 2);
    let entries_before = entry_count(&conn).unwrap();

    assert!(record_payment(&mut conn, invoice_id, 100).is_err());
    assert_eq!(entry_count(&conn).unwrap(), entries_before);
    assert!(list_payments_for_invoice(&conn, invoice_id)
        .unwrap()
        .is_empty());
}

#[test]
fn overpayment_is_rejected_and_posts_nothing() {
    let mut conn = open_db(":memory:").unwrap();
    // 2 × $125.00 = $250.00 invoice.
    let invoice_id = issued_invoice(&mut conn, "Delta", 2);
    let entries_before = entry_count(&conn).unwrap();

    let err = record_payment(&mut conn, invoice_id, 25_001).unwrap_err();
    assert!(err.to_string().contains("exceeds"), "got: {err}");
    assert_eq!(entry_count(&conn).unwrap(), entries_before);
    assert!(list_payments_for_invoice(&conn, invoice_id)
        .unwrap()
        .is_empty());
    assert_eq!(
        get_invoice(&conn, invoice_id).unwrap().unwrap().status,
        "issued"
    );
}

// ---------------------------------------------------------------------------
// Sales returns: quantity discipline and draft refusal.
// ---------------------------------------------------------------------------

#[test]
fn return_exceeding_invoiced_quantity_is_rejected() {
    let mut conn = open_db(":memory:").unwrap();
    let invoice_id = issued_invoice(&mut conn, "Acme", 5);
    let entries_before = entry_count(&conn).unwrap();
    let item = bracket_id(&conn);

    // More than invoiced: refused, nothing posts.
    let err = record_sales_return(
        &mut conn,
        invoice_id,
        &[ReturnLine {
            item_id: item,
            qty: 6,
        }],
    )
    .unwrap_err();
    assert!(err.to_string().contains("exceeds"), "got: {err}");
    assert_eq!(entry_count(&conn).unwrap(), entries_before);

    // A partial return posts; a second return past the cumulative
    // invoiced quantity is refused too.
    record_sales_return(
        &mut conn,
        invoice_id,
        &[ReturnLine {
            item_id: item,
            qty: 3,
        }],
    )
    .unwrap();
    assert_eq!(entry_count(&conn).unwrap(), entries_before + 2);
    assert!(record_sales_return(
        &mut conn,
        invoice_id,
        &[ReturnLine {
            item_id: item,
            qty: 3,
        }],
    )
    .is_err());
    assert_eq!(entry_count(&conn).unwrap(), entries_before + 2);
}

#[test]
fn return_against_draft_invoice_is_rejected() {
    let mut conn = open_db(":memory:").unwrap();
    let (_, invoice_id) = draft_invoice(&mut conn, "Beta", 5);
    let entries_before = entry_count(&conn).unwrap();
    let item = bracket_id(&conn);
    let err = record_sales_return(
        &mut conn,
        invoice_id,
        &[ReturnLine {
            item_id: item,
            qty: 1,
        }],
    )
    .unwrap_err();
    assert!(err.to_string().contains("draft"), "got: {err}");
    assert_eq!(entry_count(&conn).unwrap(), entries_before);
}

// ---------------------------------------------------------------------------
// The addendum rule: a CONFIGURED invoice posts COGS from its rolled-up
// line snapshot ($91.00/unit), not the item master ($68.00/unit).
// ---------------------------------------------------------------------------

#[test]
fn configured_invoice_posts_cogs_from_the_line_snapshot() {
    let mut conn = open_db(":memory:").unwrap();
    // Steel + galvanized, qty 50: mounting is forced to reinforced.
    // Price rolls up to $162.00, standard cost to $91.00 per unit.
    let new_quote = NewQuote {
        customer_id: customer_id(&conn, "Acme"),
        lines: vec![NewQuoteLine {
            item_id: bracket_id(&conn),
            qty: 50,
            configuration: Some(config(&[
                ("material", code("steel")),
                ("finish", code("galvanized")),
            ])),
        }],
    };
    let quote_id = create_quote(&mut conn, &new_quote).unwrap();
    send_quote(&mut conn, quote_id).unwrap();
    accept_quote(&mut conn, quote_id).unwrap();
    let order_id = create_order_from_quote(&mut conn, quote_id).unwrap();
    let invoice_id = create_invoice_from_order(&mut conn, order_id).unwrap();

    let invoice = get_invoice(&conn, invoice_id).unwrap().unwrap();
    assert_eq!(invoice.lines[0].unit_price_cents, 16_200);
    assert_eq!(invoice.lines[0].standard_cost_cents, Some(9_100));

    issue_invoice(&mut conn, invoice_id).unwrap();
    let invoice = get_invoice(&conn, invoice_id).unwrap().unwrap();

    let revenue = get_entry(&conn, invoice.gl_entry_id.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(revenue.lines[0].account_code, "1100");
    assert_eq!(revenue.lines[0].debit_cents, 50 * 16_200); // $8,100.00
    assert_eq!(revenue.lines[1].account_code, "4000");
    assert_eq!(revenue.lines[1].credit_cents, 50 * 16_200);

    let cogs = get_entry(&conn, invoice.cogs_entry_id.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(cogs.lines[0].account_code, "5000");
    // 50 × $91.00 snapshot — NOT 50 × $68.00 from the item master.
    assert_eq!(cogs.lines[0].debit_cents, 50 * 9_100);
    assert_ne!(cogs.lines[0].debit_cents, 50 * STANDARD_COST_CENTS);
    assert_eq!(cogs.lines[1].account_code, "1220");
    assert_eq!(cogs.lines[1].credit_cents, 50 * 9_100);
}

// ---------------------------------------------------------------------------
// Account ledger view: running balances in entry order.
// ---------------------------------------------------------------------------

#[test]
fn account_ledger_tracks_the_running_balance() {
    let mut conn = open_db(":memory:").unwrap();
    // AR: opening $12,400.00; +$1,250.00 invoice (10 × $125); −$1,250.00 paid.
    let invoice_id = issued_invoice(&mut conn, "Beta", 10);
    record_payment(&mut conn, invoice_id, 10 * BRACKET_UNIT_PRICE_CENTS).unwrap();

    let ledger = account_ledger(&conn, "1100").unwrap();
    assert_eq!(ledger.code, "1100");
    assert_eq!(ledger.name, "Accounts Receivable");
    assert_eq!(ledger.lines.len(), 3);
    assert_eq!(ledger.lines[0].debit_cents, 1_240_000);
    assert_eq!(ledger.lines[0].source, "opening");
    assert_eq!(ledger.lines[0].running_balance_cents, 1_240_000);
    assert_eq!(ledger.lines[1].debit_cents, 125_000);
    assert_eq!(
        ledger.lines[1].source,
        format!("invoice:{invoice_id}"),
        "the revenue posting is the second AR line"
    );
    assert_eq!(ledger.lines[1].running_balance_cents, 1_365_000);
    assert_eq!(ledger.lines[2].credit_cents, 125_000);
    assert_eq!(ledger.lines[2].running_balance_cents, 1_240_000);
    assert_eq!(ledger.balance_cents, 1_240_000);

    // An account nobody touched has an empty ledger at zero.
    let prepaid = account_ledger(&conn, "1120").unwrap();
    assert!(prepaid.lines.is_empty());
    assert_eq!(prepaid.balance_cents, 0);

    // Unknown codes are an error, not an empty ledger.
    let err = account_ledger(&conn, "9999").unwrap_err();
    assert!(matches!(err, LedgerError::UnknownAccount(_)), "got: {err}");
}
