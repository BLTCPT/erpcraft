//! ERPCraft procurement (procure-to-pay) tests — enterprise build
//! (ENTERPRISE_GAPS.md §3).
//!
//! The full P2P lifecycle with per-account posting assertions, the
//! three-way match as the centerpiece: price and quantity variances
//! block payment, a recorded override releases it, and receiving the
//! balance auto-clears a quantity hold. GR-IR nets to zero across the
//! cycle by construction — these tests prove it, step by step.
//!
//! Opening balances (frozen demo data): Cash 1000 Dr $48,200.00,
//! Raw Materials 1200 Dr $8,600.00, Finished Goods 1220 Dr $9,300.00,
//! Accounts Payable 2000 Cr $9,700.00; trial balance $120,500.00/side.

use erpcraft_core::{
    approve_requisition, cancel_purchase_order, cancel_requisition, create_purchase_order,
    create_requisition, create_supplier, get_ap_invoice, get_procurement_order, get_requisition,
    list_ap_payments_for_invoice, list_goods_receipts, list_suppliers, open_db, override_ap_match,
    pay_ap_invoice, receive_goods, record_ap_invoice, reject_requisition,
    seed_procurement_if_empty, trial_balance, ApLineInput, NewApInvoice, NewProcurementOrder,
    NewRequisition, ProcLineInput, ReceiveLineInput, ReqLineInput,
};

/// Signed balance of one account (debits − credits), in cents.
fn account_balance(conn: &rusqlite::Connection, code: &str) -> i64 {
    trial_balance(conn)
        .expect("trial balance")
        .rows
        .into_iter()
        .find(|r| r.code == code)
        .map(|r| r.debit_cents - r.credit_cents)
        .unwrap_or(0)
}

/// A seeded P2P database: opening ledger + GR-IR/PPV accounts +
/// demo suppliers. Returns (conn, SteelWorks supplier id).
fn seeded() -> (rusqlite::Connection, i64) {
    let conn = open_db(":memory:").expect("open in-memory db");
    seed_procurement_if_empty(&conn).expect("seed procurement");
    let supplier = list_suppliers(&conn)
        .expect("suppliers")
        .into_iter()
        .find(|s| s.name == "SteelWorks Supply")
        .expect("SteelWorks seeded")
        .id;
    (conn, supplier)
}

fn insert_item(conn: &rusqlite::Connection, sku: &str, make_or_buy: &str) -> i64 {
    conn.execute(
        "INSERT INTO items (sku, name, unit_price_cents, standard_cost_cents, make_or_buy) \
         VALUES (?1, ?2, 1500, 1200, ?3)",
        rusqlite::params![sku, format!("Item {sku}"), make_or_buy],
    )
    .expect("insert item");
    conn.last_insert_rowid()
}

/// A one-line PO helper. Returns (po_id, po_line_id).
fn po(
    conn: &mut rusqlite::Connection,
    supplier: i64,
    item: i64,
    qty: i64,
    price: i64,
) -> (i64, i64) {
    let id = create_purchase_order(
        conn,
        &NewProcurementOrder {
            supplier_id: Some(supplier),
            requisition_id: None,
            lines: vec![ProcLineInput {
                item_id: item,
                qty,
                unit_price_cents: price,
            }],
        },
    )
    .expect("create PO");
    let order = get_procurement_order(conn, id).unwrap().unwrap();
    (id, order.lines[0].id)
}

#[test]
fn seed_adds_grir_and_ppv_and_the_oracle_opening_is_untouched() {
    let (conn, _) = seeded();
    // The two P2P accounts exist with zero balances; the opening
    // trial balance still totals $120,500.00 per side.
    let tb = trial_balance(&conn).expect("trial balance");
    assert!(tb.rows.iter().any(|r| r.code == "2050"));
    assert!(tb.rows.iter().any(|r| r.code == "6040"));
    assert_eq!(tb.total_debit_cents, 12_050_000);
    assert_eq!(tb.total_credit_cents, 12_050_000);
    assert_eq!(account_balance(&conn, "1000"), 4_820_000);
    assert_eq!(account_balance(&conn, "2050"), 0);
    assert_eq!(account_balance(&conn, "6040"), 0);
}

#[test]
fn full_p2p_lifecycle_posts_each_step_and_grir_nets_to_zero() {
    let (mut conn, supplier) = seeded();
    let item = insert_item(&conn, "STL-ROD", "buy");
    // PO: 100 rods @ $12.00 = $1,200.00.
    let (po_id, line) = po(&mut conn, supplier, item, 100, 1200);
    let order = get_procurement_order(&conn, po_id).unwrap().unwrap();
    assert_eq!(order.status, "open");
    assert_eq!(order.total_cents, 120_000);

    // Partial receipt 60: Dr 1200 $720.00 / Cr 2050 $720.00.
    let receipt_id = receive_goods(
        &mut conn,
        po_id,
        &[ReceiveLineInput {
            po_line_id: line,
            qty: 60,
        }],
    )
    .expect("receive 60");
    assert_eq!(account_balance(&conn, "1200"), 860_000 + 72_000);
    assert_eq!(account_balance(&conn, "2050"), -72_000);
    let receipts = list_goods_receipts(&conn, Some(po_id)).unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].id, receipt_id);
    assert_eq!(receipts[0].lines[0].qty, 60);
    assert!(receipts[0].gl_entry_id.is_some());
    // Stock moved on hand.
    let on_hand: i64 = conn
        .query_row(
            "SELECT on_hand FROM stock_levels WHERE item_id = ?1",
            [item],
            |r| r.get(0),
        )
        .expect("stock row");
    assert_eq!(on_hand, 60);
    // PO partially received.
    let order = get_procurement_order(&conn, po_id).unwrap().unwrap();
    assert_eq!(order.status, "partially_received");
    assert_eq!(order.received_cents, 72_000);

    // Remainder 40: GR-IR to $1,200.00, PO received.
    receive_goods(
        &mut conn,
        po_id,
        &[ReceiveLineInput {
            po_line_id: line,
            qty: 40,
        }],
    )
    .expect("receive 40");
    assert_eq!(account_balance(&conn, "1200"), 860_000 + 120_000);
    assert_eq!(account_balance(&conn, "2050"), -120_000);
    assert_eq!(
        get_procurement_order(&conn, po_id).unwrap().unwrap().status,
        "received"
    );

    // Invoice at PO price for all 100: Dr 2050 / Cr 2000 $1,200.00.
    let ap_id = record_ap_invoice(
        &mut conn,
        &NewApInvoice {
            po_id,
            lines: vec![ApLineInput {
                po_line_id: line,
                qty: 100,
                unit_price_cents: 1200,
            }],
        },
    )
    .expect("record AP invoice");
    assert_eq!(account_balance(&conn, "2050"), 0, "GR-IR clears");
    assert_eq!(account_balance(&conn, "2000"), -970_000 - 120_000);
    assert_eq!(account_balance(&conn, "6040"), 0, "no variance at PO price");
    let invoice = get_ap_invoice(&conn, ap_id).unwrap().unwrap();
    assert_eq!(invoice.status, "open");
    assert_eq!(invoice.match_status, "matched");
    assert!(invoice.variances.is_empty());
    assert_eq!(invoice.balance_cents, 120_000);

    // Partial payment $500.00, then the balance.
    pay_ap_invoice(&mut conn, ap_id, 50_000).expect("pay part");
    assert_eq!(account_balance(&conn, "2000"), -970_000 - 70_000);
    assert_eq!(account_balance(&conn, "1000"), 4_820_000 - 50_000);
    assert_eq!(
        get_ap_invoice(&conn, ap_id).unwrap().unwrap().status,
        "partially_paid"
    );
    pay_ap_invoice(&mut conn, ap_id, 70_000).expect("pay balance");
    assert_eq!(
        account_balance(&conn, "2000"),
        -970_000,
        "AP back to opening"
    );
    assert_eq!(account_balance(&conn, "1000"), 4_820_000 - 120_000);
    assert_eq!(
        get_ap_invoice(&conn, ap_id).unwrap().unwrap().status,
        "paid"
    );
    assert_eq!(list_ap_payments_for_invoice(&conn, ap_id).unwrap().len(), 2);
    // The whole book balances.
    let tb = trial_balance(&conn).unwrap();
    assert_eq!(tb.total_debit_cents, tb.total_credit_cents);
}

#[test]
fn make_item_receipt_posts_to_finished_goods_1220_not_raw_materials() {
    let (mut conn, supplier) = seeded();
    let item = insert_item(&conn, "ASM-200", "make");
    let (po_id, line) = po(&mut conn, supplier, item, 10, 6_800);
    receive_goods(
        &mut conn,
        po_id,
        &[ReceiveLineInput {
            po_line_id: line,
            qty: 10,
        }],
    )
    .expect("receive make item");
    assert_eq!(account_balance(&conn, "1220"), 930_000 + 68_000);
    assert_eq!(account_balance(&conn, "1200"), 860_000, "1200 untouched");
    assert_eq!(account_balance(&conn, "2050"), -68_000);
}

#[test]
fn zero_value_receipt_posts_no_entry_but_moves_stock() {
    let (mut conn, supplier) = seeded();
    let item = insert_item(&conn, "FREE-SAMPLE", "buy");
    let (po_id, line) = po(&mut conn, supplier, item, 5, 0);
    let receipt_id = receive_goods(
        &mut conn,
        po_id,
        &[ReceiveLineInput {
            po_line_id: line,
            qty: 5,
        }],
    )
    .expect("receive free goods");
    let receipts = list_goods_receipts(&conn, Some(po_id)).unwrap();
    assert_eq!(receipts[0].id, receipt_id);
    assert_eq!(receipts[0].gl_entry_id, None);
    assert_eq!(account_balance(&conn, "2050"), 0);
    let on_hand: i64 = conn
        .query_row(
            "SELECT on_hand FROM stock_levels WHERE item_id = ?1",
            [item],
            |r| r.get(0),
        )
        .expect("stock row");
    assert_eq!(on_hand, 5);
    assert_eq!(
        get_procurement_order(&conn, po_id).unwrap().unwrap().status,
        "received"
    );
}

#[test]
fn price_variance_holds_payment_and_override_releases_it_with_a_record() {
    let (mut conn, supplier) = seeded();
    let item = insert_item(&conn, "STL-ROD", "buy");
    let (po_id, line) = po(&mut conn, supplier, item, 100, 1200);
    receive_goods(
        &mut conn,
        po_id,
        &[ReceiveLineInput {
            po_line_id: line,
            qty: 100,
        }],
    )
    .expect("receive all");

    // Invoice at $13.00 against a $12.00 PO: $100.00 over.
    let ap_id = record_ap_invoice(
        &mut conn,
        &NewApInvoice {
            po_id,
            lines: vec![ApLineInput {
                po_line_id: line,
                qty: 100,
                unit_price_cents: 1300,
            }],
        },
    )
    .expect("record AP invoice");
    // Posting: Dr 2050 $1,200.00 + Dr 6040 PPV $100.00 / Cr 2000 $1,300.00.
    assert_eq!(account_balance(&conn, "2050"), 0);
    assert_eq!(account_balance(&conn, "6040"), 10_000);
    assert_eq!(account_balance(&conn, "2000"), -970_000 - 130_000);
    // Match: ON HOLD with a price variance spelled out.
    let invoice = get_ap_invoice(&conn, ap_id).unwrap().unwrap();
    assert_eq!(invoice.status, "on_hold");
    assert_eq!(invoice.match_status, "variance");
    assert_eq!(invoice.variances.len(), 1);
    assert_eq!(invoice.variances[0].kind, "price");
    assert!(invoice.variances[0]
        .message
        .contains("invoiced at 1300 cents"));

    // Payment is refused while on hold.
    let err = pay_ap_invoice(&mut conn, ap_id, 130_000).unwrap_err();
    assert!(err.to_string().contains("three-way-match hold"), "{err}");

    // Override without a reason or a name is refused.
    assert!(override_ap_match(&conn, ap_id, "", "pat").is_err());
    assert!(override_ap_match(&conn, ap_id, "reason", "  ").is_err());

    // Override with both: payable, and the record is on the invoice.
    override_ap_match(
        &conn,
        ap_id,
        "Supplier fuel surcharge accepted under contract amendment 3",
        "pat",
    )
    .expect("override");
    let invoice = get_ap_invoice(&conn, ap_id).unwrap().unwrap();
    assert_eq!(invoice.status, "open");
    assert_eq!(invoice.match_status, "overridden");
    assert_eq!(
        invoice.override_reason.as_deref(),
        Some("Supplier fuel surcharge accepted under contract amendment 3")
    );
    assert_eq!(invoice.overridden_by.as_deref(), Some("pat"));
    // The variance facts are still visible after the override.
    assert_eq!(invoice.variances.len(), 1);

    pay_ap_invoice(&mut conn, ap_id, 130_000).expect("pay after override");
    assert_eq!(
        get_ap_invoice(&conn, ap_id).unwrap().unwrap().status,
        "paid"
    );
    assert_eq!(account_balance(&conn, "2000"), -970_000);
}

#[test]
fn quantity_variance_holds_and_receiving_the_balance_clears_the_hold() {
    let (mut conn, supplier) = seeded();
    let item = insert_item(&conn, "STL-ROD", "buy");
    let (po_id, line) = po(&mut conn, supplier, item, 100, 1200);
    receive_goods(
        &mut conn,
        po_id,
        &[ReceiveLineInput {
            po_line_id: line,
            qty: 60,
        }],
    )
    .expect("receive 60");

    // Supplier bills for all 100 before the rest arrive.
    let ap_id = record_ap_invoice(
        &mut conn,
        &NewApInvoice {
            po_id,
            lines: vec![ApLineInput {
                po_line_id: line,
                qty: 100,
                unit_price_cents: 1200,
            }],
        },
    )
    .expect("record AP invoice");
    let invoice = get_ap_invoice(&conn, ap_id).unwrap().unwrap();
    assert_eq!(invoice.status, "on_hold");
    assert!(invoice.variances.iter().any(|v| v.kind == "quantity"));
    assert!(invoice
        .variances
        .iter()
        .any(|v| v.message.contains("but only 60 received")));
    assert!(pay_ap_invoice(&mut conn, ap_id, 1).is_err());

    // The rest arrive: the hold clears itself, no override needed.
    receive_goods(
        &mut conn,
        po_id,
        &[ReceiveLineInput {
            po_line_id: line,
            qty: 40,
        }],
    )
    .expect("receive 40");
    let invoice = get_ap_invoice(&conn, ap_id).unwrap().unwrap();
    assert_eq!(invoice.status, "open");
    assert_eq!(invoice.match_status, "matched");
    assert!(invoice.variances.is_empty());
    pay_ap_invoice(&mut conn, ap_id, 120_000).expect("pay after clearance");
    assert_eq!(account_balance(&conn, "2050"), 0);
    assert_eq!(account_balance(&conn, "2000"), -970_000);
}

#[test]
fn overpayment_over_receipt_and_over_invoice_are_all_refused() {
    let (mut conn, supplier) = seeded();
    let item = insert_item(&conn, "STL-ROD", "buy");
    let (po_id, line) = po(&mut conn, supplier, item, 10, 1200);

    // Receiving more than ordered.
    let err = receive_goods(
        &mut conn,
        po_id,
        &[ReceiveLineInput {
            po_line_id: line,
            qty: 11,
        }],
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("exceed the ordered quantity"),
        "{err}"
    );
    receive_goods(
        &mut conn,
        po_id,
        &[ReceiveLineInput {
            po_line_id: line,
            qty: 10,
        }],
    )
    .expect("receive 10");

    // Invoicing more than ordered (a billing error, not a variance).
    let err = record_ap_invoice(
        &mut conn,
        &NewApInvoice {
            po_id,
            lines: vec![ApLineInput {
                po_line_id: line,
                qty: 11,
                unit_price_cents: 1200,
            }],
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("billing error"), "{err}");

    let ap_id = record_ap_invoice(
        &mut conn,
        &NewApInvoice {
            po_id,
            lines: vec![ApLineInput {
                po_line_id: line,
                qty: 10,
                unit_price_cents: 1200,
            }],
        },
    )
    .expect("record AP invoice");

    // Overpayment by a cent.
    let err = pay_ap_invoice(&mut conn, ap_id, 12_001).unwrap_err();
    assert!(err.to_string().contains("exceeds the balance"), "{err}");
    // Zero payment.
    assert!(pay_ap_invoice(&mut conn, ap_id, 0).is_err());
    // Cancel a PO with receipts against it.
    assert!(cancel_purchase_order(&conn, po_id).is_err());
}

#[test]
fn cancel_an_untouched_open_order_works_and_blocks_receipts() {
    let (mut conn, supplier) = seeded();
    let item = insert_item(&conn, "STL-ROD", "buy");
    let (po_id, line) = po(&mut conn, supplier, item, 10, 1200);
    cancel_purchase_order(&conn, po_id).expect("cancel open PO");
    assert_eq!(
        get_procurement_order(&conn, po_id).unwrap().unwrap().status,
        "cancelled"
    );
    let err = receive_goods(
        &mut conn,
        po_id,
        &[ReceiveLineInput {
            po_line_id: line,
            qty: 1,
        }],
    )
    .unwrap_err();
    assert!(err.to_string().contains("cancelled"), "{err}");
}

#[test]
fn requisition_lifecycle_and_conversion_to_a_purchase_order() {
    let (mut conn, supplier) = seeded();
    let item = insert_item(&conn, "STL-ROD", "buy");
    let req_id = create_requisition(
        &conn,
        &NewRequisition {
            supplier_id: Some(supplier),
            requested_by: "ops".to_string(),
            lines: vec![ReqLineInput {
                item_id: item,
                qty: 50,
                unit_price_cents: 1250,
            }],
        },
    )
    .expect("create requisition");
    let req = get_requisition(&conn, req_id).unwrap().unwrap();
    assert_eq!(req.status, "draft");
    assert_eq!(req.total_cents, 62_500);
    assert_eq!(req.requested_by, "ops");

    // A draft cannot convert.
    let err = create_purchase_order(
        &mut conn,
        &NewProcurementOrder {
            supplier_id: None,
            requisition_id: Some(req_id),
            lines: vec![],
        },
    )
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("only an approved requisition converts"),
        "{err}"
    );

    approve_requisition(&conn, req_id).expect("approve");
    // Double-approval is refused.
    assert!(approve_requisition(&conn, req_id).is_err());

    // Convert with defaults from the requisition.
    let po_id = create_purchase_order(
        &mut conn,
        &NewProcurementOrder {
            supplier_id: None,
            requisition_id: Some(req_id),
            lines: vec![],
        },
    )
    .expect("convert");
    let order = get_procurement_order(&conn, po_id).unwrap().unwrap();
    assert_eq!(order.supplier_id, supplier);
    assert_eq!(order.lines.len(), 1);
    assert_eq!(order.lines[0].qty, 50);
    assert_eq!(order.lines[0].unit_price_cents, 1250);
    assert_eq!(order.requisition_id, Some(req_id));
    assert_eq!(
        get_requisition(&conn, req_id).unwrap().unwrap().status,
        "converted"
    );

    // Reject and cancel paths.
    let req2 = create_requisition(
        &conn,
        &NewRequisition {
            supplier_id: None,
            requested_by: String::new(),
            lines: vec![ReqLineInput {
                item_id: item,
                qty: 1,
                unit_price_cents: 100,
            }],
        },
    )
    .expect("create requisition 2");
    reject_requisition(&conn, req2).expect("reject");
    assert_eq!(
        get_requisition(&conn, req2).unwrap().unwrap().status,
        "rejected"
    );
    // A rejected requisition cannot be cancelled.
    assert!(cancel_requisition(&conn, req2).is_err());

    let req3 = create_requisition(
        &conn,
        &NewRequisition {
            supplier_id: None,
            requested_by: String::new(),
            lines: vec![ReqLineInput {
                item_id: item,
                qty: 1,
                unit_price_cents: 100,
            }],
        },
    )
    .expect("create requisition 3");
    cancel_requisition(&conn, req3).expect("cancel");
    assert_eq!(
        get_requisition(&conn, req3).unwrap().unwrap().status,
        "cancelled"
    );
}

#[test]
fn supplier_names_are_unique_and_blank_names_refused() {
    let (conn, supplier) = seeded();
    let err = create_supplier(&conn, "SteelWorks Supply").unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    assert!(create_supplier(&conn, "  ").is_err());
    let id = create_supplier(&conn, "Harbor Components").expect("create supplier");
    assert!(id > supplier);
}
