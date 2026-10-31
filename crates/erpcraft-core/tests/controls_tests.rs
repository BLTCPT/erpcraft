//! ERPCraft controls tests — enterprise build (ENTERPRISE_GAPS.md
//! §6 period close + §7 identity/approvals).
//!
//! Two guarantees under test:
//! 1. A closed fiscal period refuses postings dated inside it —
//!    manual entries and document postings alike, because the gate
//!    sits in `post_entry_in`, the one posting path. Open periods
//!    and period-free databases (the frozen demo default) are
//!    unaffected.
//! 2. With an approval threshold set, a payment above it needs an
//!    actor and a *different* approver with role approver/admin;
//!    the approver is recorded on the payment row and in the posted
//!    entry's memo. Without a threshold, payments behave exactly as
//!    they always have.

use erpcraft_core::{
    accept_quote, close_fiscal_period, create_fiscal_period, create_invoice_from_order,
    create_order_from_quote, create_purchase_order, create_quote, create_user, get_entry,
    issue_invoice, list_control_settings, list_customers, list_fiscal_periods, list_items,
    list_payments_for_invoice, open_db, pay_ap_invoice_attributed, post_entry, receive_goods,
    record_ap_invoice, record_payment_attributed, seed_procurement_if_empty,
    set_approval_threshold, ApLineInput, NewApInvoice, NewEntry, NewLine, NewProcurementOrder,
    NewQuote, NewQuoteLine, ProcLineInput, ReceiveLineInput,
};

fn manual_entry(date: &str) -> NewEntry {
    NewEntry {
        entry_date: date.to_string(),
        memo: "Controls test entry".to_string(),
        source: "manual".to_string(),
        reverses_entry_id: None,
        lines: vec![
            NewLine {
                account_code: "6030".to_string(),
                debit_cents: 1_000,
                credit_cents: 0,
            },
            NewLine {
                account_code: "1000".to_string(),
                debit_cents: 0,
                credit_cents: 1_000,
            },
        ],
    }
}

/// An issued invoice for 10 × BRK-100 @ $125.00 = $1,250.00.
fn issued_invoice(conn: &mut rusqlite::Connection) -> i64 {
    let customer_id = list_customers(conn)
        .unwrap()
        .into_iter()
        .find(|c| c.name == "Acme")
        .unwrap()
        .id;
    let item_id = list_items(conn)
        .unwrap()
        .into_iter()
        .find(|i| i.sku == "BRK-100")
        .unwrap()
        .id;
    let quote_id = create_quote(
        conn,
        &NewQuote {
            customer_id,
            lines: vec![NewQuoteLine {
                item_id,
                qty: 10,
                configuration: None,
            }],
        },
    )
    .unwrap();
    erpcraft_core::send_quote(conn, quote_id).unwrap();
    accept_quote(conn, quote_id).unwrap();
    let order_id = create_order_from_quote(conn, quote_id).unwrap();
    let invoice_id = create_invoice_from_order(conn, order_id).unwrap();
    issue_invoice(conn, invoice_id).unwrap();
    invoice_id
}

fn two_users(conn: &rusqlite::Connection) {
    create_user(conn, "sam", "clerk").unwrap();
    create_user(conn, "pat", "approver").unwrap();
}

#[test]
fn closed_period_refuses_manual_entries_dated_inside_it() {
    let mut conn = open_db(":memory:").unwrap();

    // No periods defined: nothing is gated (the frozen default).
    post_entry(&mut conn, &manual_entry("2026-09-15")).unwrap();

    let period_id = create_fiscal_period(&conn, "2026-09", "2026-09-01", "2026-09-30").unwrap();
    // Still open: posting works.
    post_entry(&mut conn, &manual_entry("2026-09-15")).unwrap();

    close_fiscal_period(&conn, period_id).unwrap();
    let periods = list_fiscal_periods(&conn).unwrap();
    assert_eq!(periods[0].status, "closed");

    let err = post_entry(&mut conn, &manual_entry("2026-09-15")).unwrap_err();
    assert!(
        err.to_string()
            .contains("fiscal period '2026-09' is closed"),
        "{err}"
    );
    // Boundary days are inside the period too.
    assert!(post_entry(&mut conn, &manual_entry("2026-09-01")).is_err());
    assert!(post_entry(&mut conn, &manual_entry("2026-09-30")).is_err());
    // Outside the period: fine.
    post_entry(&mut conn, &manual_entry("2026-10-01")).unwrap();

    // Closing twice is refused; overlapping periods are refused.
    assert!(close_fiscal_period(&conn, period_id).is_err());
    assert!(create_fiscal_period(&conn, "2026-09-b", "2026-09-15", "2026-10-15").is_err());
    assert!(create_fiscal_period(&conn, "bad", "2026-13-40", "2026-13-41").is_err());
    assert!(create_fiscal_period(&conn, "backwards", "2026-11-30", "2026-11-01").is_err());
}

#[test]
fn closed_period_also_gates_document_postings_and_rolls_them_back() {
    let mut conn = open_db(":memory:").unwrap();
    seed_procurement_if_empty(&conn).unwrap();
    let supplier = erpcraft_core::list_suppliers(&conn).unwrap()[0].id;
    let item_id = list_items(&conn).unwrap()[0].id;
    let po_id = create_purchase_order(
        &mut conn,
        &NewProcurementOrder {
            supplier_id: Some(supplier),
            requisition_id: None,
            lines: vec![ProcLineInput {
                item_id,
                qty: 5,
                unit_price_cents: 1200,
            }],
        },
    )
    .unwrap();
    let po_line = erpcraft_core::get_procurement_order(&conn, po_id)
        .unwrap()
        .unwrap()
        .lines[0]
        .id;

    // A period spanning today (document postings are dated by their
    // created_at day), closed before the receipt is attempted.
    let (start, end) = conn
        .query_row(
            "SELECT date('now', '-2 days'), date('now', '+2 days')",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .unwrap();
    let period_id = create_fiscal_period(&conn, "freeze-window", &start, &end).unwrap();
    close_fiscal_period(&conn, period_id).unwrap();

    let err = receive_goods(
        &mut conn,
        po_id,
        &[ReceiveLineInput {
            po_line_id: po_line,
            qty: 5,
        }],
    )
    .unwrap_err();
    assert!(err.to_string().contains("is closed"), "{err}");
    // The whole receipt rolled back: no stock, order untouched.
    let order = erpcraft_core::get_procurement_order(&conn, po_id)
        .unwrap()
        .unwrap();
    assert_eq!(order.status, "open");
    assert_eq!(order.lines[0].qty_received, 0);
}

#[test]
fn approval_rule_is_off_until_a_threshold_is_set() {
    let mut conn = open_db(":memory:").unwrap();
    let invoice_id = issued_invoice(&mut conn);
    // No users, no threshold: an unattributed full payment works,
    // exactly as before the controls build.
    record_payment_attributed(&mut conn, invoice_id, 125_000, None, None).unwrap();
    let payments = list_payments_for_invoice(&conn, invoice_id).unwrap();
    assert_eq!(payments[0].approved_by, None);
}

#[test]
fn above_threshold_ar_payment_needs_a_second_approver_and_records_them() {
    let mut conn = open_db(":memory:").unwrap();
    two_users(&conn);
    create_user(&conn, "kim", "clerk").unwrap();
    set_approval_threshold(&conn, 100_000).unwrap();
    assert_eq!(list_control_settings(&conn).unwrap()[0].value, "100000");

    let invoice_id = issued_invoice(&mut conn);

    // At the threshold exactly: no approval needed (rule is *above*).
    let small_invoice = issued_invoice(&mut conn);
    record_payment_attributed(&mut conn, small_invoice, 100_000, None, None).unwrap();

    // Above the threshold with nobody named: refused.
    let err = record_payment_attributed(&mut conn, invoice_id, 125_000, None, None).unwrap_err();
    assert!(err.to_string().contains("approval threshold"), "{err}");
    // Actor without approver: refused.
    assert!(record_payment_attributed(&mut conn, invoice_id, 125_000, Some("sam"), None).is_err());
    // Self-approval: refused (segregation of duties).
    let err = record_payment_attributed(&mut conn, invoice_id, 125_000, Some("pat"), Some("pat"))
        .unwrap_err();
    assert!(err.to_string().contains("different person"), "{err}");
    // A clerk cannot approve.
    let err = record_payment_attributed(&mut conn, invoice_id, 125_000, Some("sam"), Some("kim"))
        .unwrap_err();
    assert!(err.to_string().contains("role 'clerk'"), "{err}");
    // Unknown people are refused on both sides.
    assert!(
        record_payment_attributed(&mut conn, invoice_id, 125_000, Some("ghost"), Some("pat"))
            .is_err()
    );
    assert!(
        record_payment_attributed(&mut conn, invoice_id, 125_000, Some("sam"), Some("ghost"))
            .is_err()
    );

    // None of the refusals recorded a payment.
    assert!(list_payments_for_invoice(&conn, invoice_id)
        .unwrap()
        .is_empty());

    // Actor (clerk) + approver (approver role): posts, and the
    // approver is on the payment row and in the entry's memo.
    let payment_id =
        record_payment_attributed(&mut conn, invoice_id, 125_000, Some("sam"), Some("pat"))
            .unwrap();
    let payments = list_payments_for_invoice(&conn, invoice_id).unwrap();
    assert_eq!(payments.len(), 1);
    assert_eq!(payments[0].id, payment_id);
    assert_eq!(payments[0].approved_by.as_deref(), Some("pat"));
    let entry = get_entry(&conn, payments[0].gl_entry_id.unwrap())
        .unwrap()
        .unwrap();
    assert!(entry.memo.contains("approved by pat"), "{}", entry.memo);
}

#[test]
fn above_threshold_ap_payment_follows_the_same_rule() {
    let mut conn = open_db(":memory:").unwrap();
    seed_procurement_if_empty(&conn).unwrap();
    two_users(&conn);
    set_approval_threshold(&conn, 100_000).unwrap();

    let supplier = erpcraft_core::list_suppliers(&conn).unwrap()[0].id;
    let item_id = list_items(&conn).unwrap()[0].id;
    let po_id = create_purchase_order(
        &mut conn,
        &NewProcurementOrder {
            supplier_id: Some(supplier),
            requisition_id: None,
            lines: vec![ProcLineInput {
                item_id,
                qty: 100,
                unit_price_cents: 1200,
            }],
        },
    )
    .unwrap();
    let po_line = erpcraft_core::get_procurement_order(&conn, po_id)
        .unwrap()
        .unwrap()
        .lines[0]
        .id;
    receive_goods(
        &mut conn,
        po_id,
        &[ReceiveLineInput {
            po_line_id: po_line,
            qty: 100,
        }],
    )
    .unwrap();
    let ap_id = record_ap_invoice(
        &mut conn,
        &NewApInvoice {
            po_id,
            lines: vec![ApLineInput {
                po_line_id: po_line,
                qty: 100,
                unit_price_cents: 1200,
            }],
        },
    )
    .unwrap();

    // $1,200.00 against a $1,000.00 threshold: unattributed refuses.
    let err = pay_ap_invoice_attributed(&mut conn, ap_id, 120_000, None, None).unwrap_err();
    assert!(err.to_string().contains("approval threshold"), "{err}");

    pay_ap_invoice_attributed(&mut conn, ap_id, 120_000, Some("sam"), Some("pat")).unwrap();
    let payments = erpcraft_core::list_ap_payments_for_invoice(&conn, ap_id).unwrap();
    assert_eq!(payments[0].approved_by.as_deref(), Some("pat"));
    let entry = get_entry(&conn, payments[0].gl_entry_id.unwrap())
        .unwrap()
        .unwrap();
    assert!(entry.memo.contains("approved by pat"), "{}", entry.memo);
}

#[test]
fn user_and_setting_validation() {
    let conn = open_db(":memory:").unwrap();
    create_user(&conn, "sam", "clerk").unwrap();
    // Duplicate name and unknown role are refused.
    assert!(create_user(&conn, "sam", "admin").is_err());
    let err = create_user(&conn, "jo", "superuser").unwrap_err();
    assert!(err.to_string().contains("unknown role"), "{err}");
    assert!(create_user(&conn, "  ", "clerk").is_err());
    // Threshold may not be negative; zero is a valid (strictest) setting.
    assert!(set_approval_threshold(&conn, -1).is_err());
    set_approval_threshold(&conn, 0).unwrap();
    assert_eq!(erpcraft_core::approval_threshold(&conn).unwrap(), Some(0));
}
