//! Day-6 audit-trail tests: every posted entry explains itself —
//! who, what document, why — and the provenance chain of an invoice
//! lifecycle (quote → order → invoice → payment → return reversal)
//! walks in both directions.

use erpcraft_core::rusqlite::Connection;
use erpcraft_core::{
    accept_quote, audit_entry, audit_trail, create_invoice_from_order, create_order_from_quote,
    create_quote, get_invoice, issue_invoice, list_customers, list_items, open_db, post_entry,
    record_payment, record_sales_return, send_quote, AuditFilter, NewEntry, NewLine, NewQuote,
    NewQuoteLine, ReturnLine, BRACKET_SKU,
};

fn customer_id(conn: &Connection, name: &str) -> i64 {
    list_customers(conn)
        .unwrap()
        .into_iter()
        .find(|c| c.name == name)
        .unwrap()
        .id
}

fn bracket_id(conn: &Connection) -> i64 {
    list_items(conn)
        .unwrap()
        .into_iter()
        .find(|i| i.sku == BRACKET_SKU)
        .unwrap()
        .id
}

/// Drive Acme through the full lifecycle: quote 10 → order →
/// invoice → issue → pay in full → return 2. Returns the invoice id.
fn full_lifecycle(conn: &mut Connection) -> i64 {
    let quote_id = create_quote(
        conn,
        &NewQuote {
            customer_id: customer_id(conn, "Acme"),
            lines: vec![NewQuoteLine {
                item_id: bracket_id(conn),
                qty: 10,
                configuration: None,
            }],
        },
    )
    .unwrap();
    send_quote(conn, quote_id).unwrap();
    accept_quote(conn, quote_id).unwrap();
    let order_id = create_order_from_quote(conn, quote_id).unwrap();
    let invoice_id = create_invoice_from_order(conn, order_id).unwrap();
    issue_invoice(conn, invoice_id).unwrap();
    record_payment(conn, invoice_id, 125_000).unwrap();
    record_sales_return(
        conn,
        invoice_id,
        &[ReturnLine {
            item_id: bracket_id(conn),
            qty: 2,
        }],
    )
    .unwrap();
    invoice_id
}

#[test]
fn invoice_lifecycle_chain_includes_the_return_reversal() {
    let mut conn = open_db(":memory:").unwrap();
    let invoice_id = full_lifecycle(&mut conn);
    let invoice = get_invoice(&conn, invoice_id).unwrap().unwrap();
    let revenue_id = invoice.gl_entry_id.unwrap();
    let cogs_id = invoice.cogs_entry_id.unwrap();

    // The revenue entry explains itself and carries the whole chain.
    let record = audit_entry(&conn, revenue_id).unwrap().unwrap();
    assert_eq!(record.who, "posting engine");
    assert_eq!(record.role, "revenue");
    let doc = record.document.as_ref().unwrap();
    assert_eq!(doc.kind, "invoice");
    assert_eq!(doc.id, invoice_id);
    let kinds: Vec<&str> = record.chain.iter().map(|n| n.kind.as_str()).collect();
    assert_eq!(
        kinds,
        ["quote", "sales_order", "invoice", "payment", "sales_return"]
    );
    let invoice_node = &record.chain[2];
    assert_eq!(invoice_node.entry_ids, vec![revenue_id, cogs_id]);
    // The return reversed this entry — visible from the original.
    let return_revenue_id = record.chain[4].entry_ids[0];
    assert_eq!(record.reversed_by, vec![return_revenue_id]);

    // The return entry points back at what it corrects.
    let reversal = audit_entry(&conn, return_revenue_id).unwrap().unwrap();
    assert_eq!(reversal.role, "return_revenue");
    assert_eq!(reversal.entry.reverses_entry_id, Some(revenue_id));
    assert_eq!(reversal.document.as_ref().unwrap().kind, "sales_return");
    assert!(reversal.summary.contains("reverses entry"));

    // The payment entry is in the same chain, with its amount.
    let payment_node = &record.chain[3];
    let payment = audit_entry(&conn, payment_node.entry_ids[0])
        .unwrap()
        .unwrap();
    assert_eq!(payment.role, "payment");
    assert!(payment.summary.contains("$1250.00"));
    assert_eq!(payment.document.as_ref().unwrap().kind, "payment");
}

#[test]
fn agent_entry_surfaces_the_verbatim_prompt() {
    let mut conn = open_db(":memory:").unwrap();
    let entry_id = post_entry(
        &mut conn,
        &NewEntry {
            entry_date: "2026-10-30".to_string(),
            memo: "Buy materials — prompt: \"Buy $3,600 of raw materials on credit.\"".to_string(),
            source: "agent".to_string(),
            reverses_entry_id: None,
            lines: vec![
                NewLine {
                    account_code: "1200".to_string(),
                    debit_cents: 360_000,
                    credit_cents: 0,
                },
                NewLine {
                    account_code: "2000".to_string(),
                    debit_cents: 0,
                    credit_cents: 360_000,
                },
            ],
        },
    )
    .unwrap();
    let record = audit_entry(&conn, entry_id).unwrap().unwrap();
    assert_eq!(record.who, "NL agent (human-confirmed)");
    assert_eq!(
        record.prompt.as_deref(),
        Some("Buy $3,600 of raw materials on credit.")
    );
    assert!(record.document.is_none());
    assert!(record.chain.is_empty());
}

#[test]
fn audit_trail_filters_by_source_document_and_date() {
    let mut conn = open_db(":memory:").unwrap();
    let invoice_id = full_lifecycle(&mut conn);
    post_entry(
        &mut conn,
        &NewEntry {
            entry_date: "2026-10-30".to_string(),
            memo: "Buy materials — prompt: \"Buy $3,600 of raw materials on credit.\"".to_string(),
            source: "agent".to_string(),
            reverses_entry_id: None,
            lines: vec![
                NewLine {
                    account_code: "1200".to_string(),
                    debit_cents: 360_000,
                    credit_cents: 0,
                },
                NewLine {
                    account_code: "2000".to_string(),
                    debit_cents: 0,
                    credit_cents: 360_000,
                },
            ],
        },
    )
    .unwrap();

    // Source filter: exactly the agent entry.
    let by_source = audit_trail(
        &conn,
        &AuditFilter {
            source: Some("agent".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(by_source.len(), 1);
    assert_eq!(by_source[0].who, "NL agent (human-confirmed)");

    // Document filter: the invoice's whole lifecycle — revenue,
    // COGS, one payment, two return entries.
    let by_doc = audit_trail(
        &conn,
        &AuditFilter {
            document: Some(format!("invoice:{invoice_id}")),
            ..Default::default()
        },
    )
    .unwrap();
    let roles: Vec<&str> = by_doc.iter().map(|r| r.role.as_str()).collect();
    assert_eq!(
        roles,
        [
            "revenue",
            "cogs",
            "payment",
            "return_revenue",
            "return_cogs"
        ]
    );

    // Date filter: nothing was posted in 2027.
    let by_date = audit_trail(
        &conn,
        &AuditFilter {
            from_date: Some("2027-01-01".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(by_date.is_empty());

    // Unfiltered: opening + 5 lifecycle + 1 agent.
    let all = audit_trail(&conn, &AuditFilter::default()).unwrap();
    assert_eq!(all.len(), 7);
    assert_eq!(all[0].role, "opening");
    assert_eq!(all[0].who, "system seed");
}
