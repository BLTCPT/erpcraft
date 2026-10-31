//! Day-2 order-to-cash tests for the ERPCraft core.
//!
//! The Day-2 contract: quote → order → invoice → payment flows through the
//! document state machines, totals are always derived from lines, illegal
//! jumps are refused. Day 3 reconciled the GL side: quotes, orders and
//! draft invoices still post nothing, but issuance and payments now post
//! exactly the mapped GL entries in the same transaction as the
//! transition — test 4 asserts those postings amount-for-amount instead
//! of the old no-GL guard.

use erpcraft_core::rusqlite::Connection;
use erpcraft_core::{
    accept_quote, create_invoice_from_order, create_order_from_quote, create_quote, customer_count,
    decline_quote, entry_count, get_invoice, get_order, get_quote, issue_invoice, item_count,
    list_customers, list_entries, list_items, list_payments_for_invoice, open_db, record_payment,
    send_quote, trial_balance, NewQuote, NewQuoteLine, BRACKET_SKU, BRACKET_UNIT_PRICE_CENTS,
    OPENING_TOTAL_CENTS, STANDARD_COST_CENTS,
};

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

fn quote_for(conn: &Connection, customer: &str, qty: i64) -> NewQuote {
    NewQuote {
        customer_id: customer_id(conn, customer),
        lines: vec![NewQuoteLine {
            item_id: bracket_id(conn),
            qty,
            configuration: None,
        }],
    }
}

/// Drive a quote all the way to an issued invoice; returns
/// (quote_id, order_id, invoice_id).
fn issued_invoice(conn: &mut Connection, customer: &str, qty: i64) -> (i64, i64, i64) {
    let new_quote = quote_for(conn, customer, qty);
    let quote_id = create_quote(conn, &new_quote).unwrap();
    send_quote(conn, quote_id).unwrap();
    accept_quote(conn, quote_id).unwrap();
    let order_id = create_order_from_quote(conn, quote_id).unwrap();
    let invoice_id = create_invoice_from_order(conn, order_id).unwrap();
    issue_invoice(conn, invoice_id).unwrap();
    (quote_id, order_id, invoice_id)
}

// ---------------------------------------------------------------------------
// 1. Full flow: quote -> accept -> order -> invoice -> payment in full.
// ---------------------------------------------------------------------------

#[test]
fn full_flow_quote_to_paid_invoice() {
    let mut conn = open_db(":memory:").unwrap();

    // Quote Acme for 40 brackets at $125.00 = $5,000.00.
    let new_quote = quote_for(&conn, "Acme", 40);
    let quote_id = create_quote(&mut conn, &new_quote).unwrap();
    let quote = get_quote(&conn, quote_id).unwrap().expect("quote exists");
    assert_eq!(quote.status, "draft");
    assert_eq!(quote.customer_name, "Acme");
    assert_eq!(quote.lines.len(), 1);
    assert_eq!(quote.lines[0].qty, 40);
    assert_eq!(quote.lines[0].unit_price_cents, BRACKET_UNIT_PRICE_CENTS);
    assert_eq!(
        quote.lines[0].line_total_cents,
        40 * BRACKET_UNIT_PRICE_CENTS
    );
    assert_eq!(quote.total_cents, 500_000); // $5,000.00
    assert_eq!(quote.total_cents, quote.lines[0].line_total_cents);

    send_quote(&mut conn, quote_id).unwrap();
    accept_quote(&mut conn, quote_id).unwrap();
    assert_eq!(
        get_quote(&conn, quote_id).unwrap().unwrap().status,
        "accepted"
    );

    // The order copies the quote's lines and links back to it.
    let order_id = create_order_from_quote(&mut conn, quote_id).unwrap();
    let order = get_order(&conn, order_id).unwrap().expect("order exists");
    assert_eq!(order.status, "open");
    assert_eq!(order.quote_id, quote_id);
    assert_eq!(order.customer_name, "Acme");
    assert_eq!(order.total_cents, quote.total_cents);
    assert_eq!(
        order.lines[0].line_total_cents,
        40 * BRACKET_UNIT_PRICE_CENTS
    );

    // The invoice copies the order's lines and links back to it.
    let invoice_id = create_invoice_from_order(&mut conn, order_id).unwrap();
    let invoice = get_invoice(&conn, invoice_id)
        .unwrap()
        .expect("invoice exists");
    assert_eq!(invoice.status, "draft");
    assert_eq!(invoice.order_id, order_id);
    assert_eq!(invoice.total_cents, 500_000);
    assert_eq!(invoice.paid_cents, 0);
    assert_eq!(invoice.balance_cents, 500_000);

    issue_invoice(&mut conn, invoice_id).unwrap();
    let payment_id = record_payment(&mut conn, invoice_id, 500_000).unwrap();

    let invoice = get_invoice(&conn, invoice_id).unwrap().unwrap();
    assert_eq!(invoice.status, "paid");
    assert_eq!(invoice.paid_cents, 500_000);
    assert_eq!(invoice.balance_cents, 0);

    let payments = list_payments_for_invoice(&conn, invoice_id).unwrap();
    assert_eq!(payments.len(), 1);
    assert_eq!(payments[0].id, payment_id);
    assert_eq!(payments[0].invoice_id, invoice_id);
    assert_eq!(payments[0].amount_cents, 500_000);
    assert_eq!(payments[0].status, "recorded");
}

// ---------------------------------------------------------------------------
// 2. Partial payment: issued -> partially_paid -> paid.
// ---------------------------------------------------------------------------

#[test]
fn partial_payment_moves_invoice_through_partially_paid() {
    let mut conn = open_db(":memory:").unwrap();
    // Beta, 10 brackets = $1,250.00.
    let (_, _, invoice_id) = issued_invoice(&mut conn, "Beta", 10);
    assert_eq!(
        get_invoice(&conn, invoice_id).unwrap().unwrap().total_cents,
        125_000
    );

    record_payment(&mut conn, invoice_id, 50_000).unwrap();
    let invoice = get_invoice(&conn, invoice_id).unwrap().unwrap();
    assert_eq!(invoice.status, "partially_paid");
    assert_eq!(invoice.paid_cents, 50_000);
    assert_eq!(invoice.balance_cents, 75_000);

    record_payment(&mut conn, invoice_id, 75_000).unwrap();
    let invoice = get_invoice(&conn, invoice_id).unwrap().unwrap();
    assert_eq!(invoice.status, "paid");
    assert_eq!(invoice.paid_cents, 125_000);
    assert_eq!(invoice.balance_cents, 0);
    assert_eq!(
        list_payments_for_invoice(&conn, invoice_id).unwrap().len(),
        2
    );
}

// ---------------------------------------------------------------------------
// 3. Illegal jumps are rejected.
// ---------------------------------------------------------------------------

#[test]
fn illegal_transitions_are_rejected() {
    let mut conn = open_db(":memory:").unwrap();

    // (a) Order from a quote that was never accepted (still draft / sent).
    let new_quote = quote_for(&conn, "Acme", 5);
    let draft_quote = create_quote(&mut conn, &new_quote).unwrap();
    let err = create_order_from_quote(&mut conn, draft_quote).unwrap_err();
    assert!(err.to_string().contains("accepted"), "got: {err}");
    send_quote(&mut conn, draft_quote).unwrap();
    assert!(create_order_from_quote(&mut conn, draft_quote).is_err());

    // (b) Order from a declined quote — and therefore no invoice can ever
    // be chained to a non-accepted quote either.
    decline_quote(&mut conn, draft_quote).unwrap();
    let err = create_order_from_quote(&mut conn, draft_quote).unwrap_err();
    assert!(err.to_string().contains("declined"), "got: {err}");

    // (c) Accepting a quote that was never sent.
    let new_quote = quote_for(&conn, "Delta", 1);
    let unsent = create_quote(&mut conn, &new_quote).unwrap();
    assert!(accept_quote(&mut conn, unsent).is_err());

    // (d) Payment on a draft invoice (created but not issued).
    let new_quote = quote_for(&conn, "Acme", 2);
    let quote_id = create_quote(&mut conn, &new_quote).unwrap();
    send_quote(&mut conn, quote_id).unwrap();
    accept_quote(&mut conn, quote_id).unwrap();
    let order_id = create_order_from_quote(&mut conn, quote_id).unwrap();
    let invoice_id = create_invoice_from_order(&mut conn, order_id).unwrap();
    let err = record_payment(&mut conn, invoice_id, 100).unwrap_err();
    assert!(err.to_string().contains("draft"), "got: {err}");

    // (e) Payment exceeding the invoice total (2 x $125.00 = $250.00).
    issue_invoice(&mut conn, invoice_id).unwrap();
    let err = record_payment(&mut conn, invoice_id, 25_001).unwrap_err();
    assert!(err.to_string().contains("exceeds"), "got: {err}");
    // A partial payment followed by an over-the-balance payment also fails.
    record_payment(&mut conn, invoice_id, 20_000).unwrap();
    assert!(record_payment(&mut conn, invoice_id, 5_001).is_err());
    // And non-positive payments are refused outright.
    assert!(record_payment(&mut conn, invoice_id, 0).is_err());
    assert!(record_payment(&mut conn, invoice_id, -100).is_err());

    // (f) A second order for the same quote, and a second payment after
    // the invoice is fully paid.
    assert!(create_order_from_quote(&mut conn, quote_id).is_err());
    record_payment(&mut conn, invoice_id, 5_000).unwrap();
    assert_eq!(
        get_invoice(&conn, invoice_id).unwrap().unwrap().status,
        "paid"
    );
    assert!(record_payment(&mut conn, invoice_id, 1).is_err());
}

// ---------------------------------------------------------------------------
// 4. DAY-3 POSTING MAP: quotes and orders post NOTHING; issuance posts
//    exactly the revenue + COGS entries; each payment posts exactly one
//    cash entry. (This test replaces the Day-2 no-GL guard, inverted for
//    issuance and payments only.)
// ---------------------------------------------------------------------------

#[test]
fn flow_posts_exactly_the_day3_mapped_entries() {
    let mut conn = open_db(":memory:").unwrap();
    assert_eq!(entry_count(&conn).unwrap(), 1, "only the opening entry");
    let tb_before = trial_balance(&conn).unwrap();
    assert_eq!(tb_before.total_debit_cents, OPENING_TOTAL_CENTS);

    // Quote → order → draft invoice: no GL movement at all.
    let new_quote = quote_for(&conn, "Epsilon", 40);
    let quote_id = create_quote(&mut conn, &new_quote).unwrap();
    send_quote(&mut conn, quote_id).unwrap();
    accept_quote(&mut conn, quote_id).unwrap();
    let order_id = create_order_from_quote(&mut conn, quote_id).unwrap();
    let invoice_id = create_invoice_from_order(&mut conn, order_id).unwrap();
    assert_eq!(entry_count(&conn).unwrap(), 1, "quotes/orders post nothing");
    assert_eq!(
        trial_balance(&conn).unwrap().total_debit_cents,
        OPENING_TOTAL_CENTS
    );

    // Issuance posts exactly two entries: revenue $5,000.00 and
    // COGS 40 × $68.00 = $2,720.00, both sourced to the invoice.
    issue_invoice(&mut conn, invoice_id).unwrap();
    assert_eq!(entry_count(&conn).unwrap(), 3);
    let invoice = get_invoice(&conn, invoice_id).unwrap().unwrap();
    let entries = list_entries(&conn).unwrap();
    let revenue = entries
        .iter()
        .find(|e| Some(e.id) == invoice.gl_entry_id)
        .expect("revenue entry recorded on the invoice");
    assert_eq!(revenue.source, format!("invoice:{invoice_id}"));
    assert_eq!(revenue.lines.len(), 2);
    assert_eq!(revenue.lines[0].account_code, "1100");
    assert_eq!(revenue.lines[0].debit_cents, 500_000);
    assert_eq!(revenue.lines[1].account_code, "4000");
    assert_eq!(revenue.lines[1].credit_cents, 500_000);
    let cogs = entries
        .iter()
        .find(|e| Some(e.id) == invoice.cogs_entry_id)
        .expect("COGS entry recorded on the invoice");
    assert_eq!(cogs.source, format!("invoice:{invoice_id}"));
    assert_eq!(cogs.lines[0].account_code, "5000");
    assert_eq!(cogs.lines[0].debit_cents, 40 * STANDARD_COST_CENTS);
    assert_eq!(cogs.lines[1].account_code, "1220");
    assert_eq!(cogs.lines[1].credit_cents, 40 * STANDARD_COST_CENTS);

    // Each payment posts exactly one entry: Dr Cash / Cr AR, its amount.
    let first = record_payment(&mut conn, invoice_id, 200_000).unwrap();
    assert_eq!(entry_count(&conn).unwrap(), 4);
    let payments = list_payments_for_invoice(&conn, invoice_id).unwrap();
    let first_entry = entries_by_id(&conn, payments[0].gl_entry_id);
    assert_eq!(first_entry.source, format!("payment:{first}"));
    assert_eq!(first_entry.lines[0].account_code, "1000");
    assert_eq!(first_entry.lines[0].debit_cents, 200_000);
    assert_eq!(first_entry.lines[1].account_code, "1100");
    assert_eq!(first_entry.lines[1].credit_cents, 200_000);
    record_payment(&mut conn, invoice_id, 300_000).unwrap();
    assert_eq!(entry_count(&conn).unwrap(), 5);
    assert_eq!(
        get_invoice(&conn, invoice_id).unwrap().unwrap().status,
        "paid"
    );

    // The trial balance moved by exactly the mapped amounts, per account.
    let tb = trial_balance(&conn).unwrap();
    let expected: &[(&str, i64, i64)] = &[
        ("1000", 5_320_000, 0), // Cash 48,200 + 5,000 paid in
        ("1100", 1_240_000, 0), // AR 12,400 + 5,000 − 5,000
        ("1120", 0, 0),
        ("1200", 860_000, 0),
        ("1210", 0, 0),
        ("1220", 658_000, 0), // Finished Goods 9,300 − 2,720 COGS
        ("1300", 4_200_000, 0),
        ("1310", 0, 680_000),
        ("2000", 0, 970_000),
        ("2100", 0, 0),
        ("2200", 0, 0),
        ("2300", 0, 2_500_000),
        ("2400", 0, 0),
        ("3000", 0, 6_500_000),
        ("3100", 0, 1_400_000),
        ("4000", 0, 500_000), // Revenue
        ("4010", 0, 0),
        ("5000", 272_000, 0), // COGS
        ("6000", 0, 0),
        ("6010", 0, 0),
        ("6020", 0, 0),
        ("6030", 0, 0),
    ];
    assert_eq!(tb.rows.len(), expected.len());
    for (row, (code, debit, credit)) in tb.rows.iter().zip(expected.iter()) {
        assert_eq!(row.code, *code);
        assert_eq!(row.debit_cents, *debit, "account {code} debit");
        assert_eq!(row.credit_cents, *credit, "account {code} credit");
    }
    assert_eq!(tb.total_debit_cents, 12_550_000);
    assert_eq!(tb.total_credit_cents, 12_550_000);
}

/// Fetch one entry by the id recorded on a document header.
fn entries_by_id(conn: &Connection, entry_id: Option<i64>) -> erpcraft_core::JournalEntryView {
    let id = entry_id.expect("a GL entry id is recorded");
    list_entries(conn)
        .unwrap()
        .into_iter()
        .find(|e| e.id == id)
        .expect("the recorded entry exists")
}

// ---------------------------------------------------------------------------
// 5. Seed determinism: master data seeded exactly once; reopen is a no-op.
// ---------------------------------------------------------------------------

#[test]
fn master_data_seeds_exactly_once() {
    let mut path = std::env::temp_dir();
    path.push(format!("erpcraft-day2-seed-test-{}.db", std::process::id()));
    let path_str = path.to_str().unwrap().to_string();
    let _ = std::fs::remove_file(&path);

    {
        let conn = open_db(&path_str).unwrap();
        assert_eq!(customer_count(&conn).unwrap(), 4);
        assert_eq!(item_count(&conn).unwrap(), 1);
        assert_eq!(entry_count(&conn).unwrap(), 1);
        let customers = list_customers(&conn).unwrap();
        let names: Vec<&str> = customers.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["Acme", "Beta", "Delta", "Epsilon"]);
        let items = list_items(&conn).unwrap();
        assert_eq!(items[0].sku, BRACKET_SKU);
        assert_eq!(items[0].unit_price_cents, BRACKET_UNIT_PRICE_CENTS);
        assert_eq!(items[0].unit_price_cents, 12_500); // $125.00
        assert_eq!(items[0].standard_cost_cents, STANDARD_COST_CENTS);
    }
    {
        // Second open: seeding is a no-op, nothing duplicates.
        let conn = open_db(&path_str).unwrap();
        assert_eq!(customer_count(&conn).unwrap(), 4);
        assert_eq!(item_count(&conn).unwrap(), 1);
        assert_eq!(entry_count(&conn).unwrap(), 1);
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn absurd_quantity_is_an_overflow_error_not_a_wrapped_total() {
    // SF2 regression (external review, Oct 31): qty x price used to be an
    // unchecked i64 multiply — wrapping silently in release builds.
    let mut conn = open_db(":memory:").unwrap();
    let quote = quote_for(&conn, "Acme", i64::MAX / 1_000);
    create_quote(&mut conn, &quote).unwrap();
    // (list_quotes, not get_quote: the getter maps Invalid to None.)
    let err = erpcraft_core::list_quotes(&conn)
        .expect_err("an absurd quantity must not produce a wrapped line total");
    assert!(
        err.to_string().contains("overflow"),
        "expected an overflow error, got: {err}"
    );
}
