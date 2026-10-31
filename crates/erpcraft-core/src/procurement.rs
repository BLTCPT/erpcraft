//! Procure-to-pay (ENTERPRISE_GAPS.md §3): the supplier-facing
//! document chain that mirrors order-to-cash —
//!
//! ```text
//! purchase requisition → procurement order → goods receipt
//!     → AP invoice (three-way match) → AP payment
//! ```
//!
//! State machines live here in core, exactly as in the O2C section of
//! `lib.rs`; every GL posting happens inside the transition's own
//! transaction via `post_entry_in`, and document headers record their
//! entry ids as double-post guards.
//!
//! Posting map:
//!
//! * **Goods receipt** — Dr inventory at PO price (1200 Raw Materials
//!   for bought items, 1220 Finished Goods for made items) /
//!   Cr 2050 GR-IR clearing. Receipts also move the MRP quantity
//!   state (`stock_levels.on_hand`), so planning and payables read
//!   the same physical truth.
//! * **AP invoice** — Dr 2050 GR-IR at PO price × invoiced qty,
//!   Dr/Cr 6040 Purchase Price Variance for the invoiced-vs-PO price
//!   difference, Cr 2000 Accounts Payable at the invoice total.
//! * **AP payment** — Dr 2000 / Cr 1000 Cash.
//!
//! Three-way match (PO vs. receipt vs. invoice) is evaluated when an
//! AP invoice is recorded and re-evaluated when more goods arrive:
//! an invoice whose cumulative invoiced quantity exceeds what has
//! been received, or whose prices differ from the PO's, is recorded
//! but lands `on_hold` — payment is refused until the variance
//! resolves (the goods arrive) or a human overrides the match with a
//! recorded reason (`override_ap_match`). Invoicing more than was
//! *ordered* is refused outright: that is not a variance, it is a
//! billing error.
//!
//! Deliberate limits (named in ENTERPRISE_GAPS.md): one PO per AP
//! invoice; no partial-line price tolerance bands — any price
//! difference is a variance; no credit memos yet (a wrong invoice is
//! corrected by override + a follow-up invoice, not edited — the
//! journal is append-only and so are these documents).

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::{
    add_cents, mul_cents, post_entry_in, sum_cents, Cents, LedgerError, NewEntry, NewLine, Result,
};

/// GR-IR clearing: goods received, invoice not yet received (and the
/// mirror when invoicing outruns receiving). Seeded by
/// [`seed_procurement_if_empty`], not by the Day-1 chart seed.
pub const ACCT_GRIR: &str = "2050";
/// Purchase price variance (invoiced price vs. PO price).
pub const ACCT_PPV: &str = "6040";
const ACCT_AP: &str = "2000";
const ACCT_CASH: &str = "1000";
const ACCT_RAW_MATERIALS: &str = "1200";
const ACCT_FINISHED_GOODS: &str = "1220";

/// The demo suppliers of Cascade Precision Parts.
const DEMO_SUPPLIERS: &[&str] = &["SteelWorks Supply", "Pacific Fasteners"];

// ---------------------------------------------------------------------------
// Seed + account ensuring
// ---------------------------------------------------------------------------

/// Seed the procure-to-pay surface: the GR-IR (2050) and PPV (6040)
/// accounts and two demo suppliers. Called at the doors (CLI/MCP open
/// path and the HTTP server boot), following the MRP demo-seed
/// precedent — the Day-1 chart seed in `open_db` is a frozen test
/// surface (the oracle suite pins its 22 accounts per row), so these
/// accounts must not arrive through it. Returns true when the
/// supplier seed ran (suppliers table was empty). Idempotent: the
/// accounts are insert-or-ignore and an existing supplier list is
/// left untouched.
pub fn seed_procurement_if_empty(conn: &Connection) -> Result<bool> {
    ensure_procurement_accounts(conn)?;
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM suppliers", [], |r| r.get(0))?;
    if count > 0 {
        return Ok(false);
    }
    for name in DEMO_SUPPLIERS {
        conn.execute("INSERT INTO suppliers (name) VALUES (?1)", params![name])?;
    }
    Ok(true)
}

/// The two P2P accounts, created if missing. Posting transitions call
/// this inside their own transaction as a safety net, so the core API
/// is correct even on a database the doors never seeded.
fn ensure_procurement_accounts(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO accounts (code, name, account_type)
         VALUES ('2050', 'Goods Received / Invoice Received (GR-IR)', 'liability')",
        [],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO accounts (code, name, account_type)
         VALUES ('6040', 'Purchase Price Variance', 'expense')",
        [],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Local helpers (mirrors of the lib.rs O2C internals)
// ---------------------------------------------------------------------------

fn missing(what: &str, id: i64) -> impl Fn(rusqlite::Error) -> LedgerError {
    let what = what.to_string();
    move |e| match e {
        rusqlite::Error::QueryReturnedNoRows => {
            LedgerError::Invalid(format!("no {what} with id {id}"))
        }
        other => LedgerError::Db(other),
    }
}

/// The date part of a `created_at` timestamp (`YYYY-MM-DD HH:MM:SS` →
/// `YYYY-MM-DD`): document postings are dated the day the document
/// was created, exactly as the O2C engine dates its entries.
fn date_part(timestamp: &str) -> String {
    timestamp.chars().take(10).collect()
}

fn supplier_name(conn: &Connection, supplier_id: i64) -> Result<String> {
    conn.query_row(
        "SELECT name FROM suppliers WHERE id = ?1",
        params![supplier_id],
        |r| r.get(0),
    )
    .map_err(missing("supplier", supplier_id))
}

fn item_sku(conn: &Connection, item_id: i64) -> Result<String> {
    conn.query_row(
        "SELECT sku FROM items WHERE id = ?1",
        params![item_id],
        |r| r.get(0),
    )
    .map_err(|_| LedgerError::Invalid(format!("unknown item id {item_id}")))
}

// ---------------------------------------------------------------------------
// Data transfer types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupplierView {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReqLineInput {
    pub item_id: i64,
    pub qty: i64,
    pub unit_price_cents: Cents,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewRequisition {
    pub supplier_id: Option<i64>,
    #[serde(default)]
    pub requested_by: String,
    pub lines: Vec<ReqLineInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReqLineView {
    pub id: i64,
    pub item_id: i64,
    pub sku: String,
    pub qty: i64,
    pub unit_price_cents: Cents,
    pub line_total_cents: Cents,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequisitionView {
    pub id: i64,
    pub supplier_id: Option<i64>,
    pub supplier_name: Option<String>,
    pub requested_by: String,
    pub status: String,
    pub lines: Vec<ReqLineView>,
    pub total_cents: Cents,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcLineInput {
    pub item_id: i64,
    pub qty: i64,
    pub unit_price_cents: Cents,
}

/// A new procurement order. With `requisition_id`, the supplier and
/// lines default to the requisition's when not given explicitly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewProcurementOrder {
    pub supplier_id: Option<i64>,
    pub requisition_id: Option<i64>,
    #[serde(default)]
    pub lines: Vec<ProcLineInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcOrderLineView {
    pub id: i64,
    pub item_id: i64,
    pub sku: String,
    pub qty: i64,
    pub unit_price_cents: Cents,
    pub line_total_cents: Cents,
    pub qty_received: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcOrderView {
    pub id: i64,
    pub supplier_id: i64,
    pub supplier_name: String,
    pub requisition_id: Option<i64>,
    pub status: String,
    pub lines: Vec<ProcOrderLineView>,
    /// Derived: sum of ordered line totals.
    pub total_cents: Cents,
    /// Derived: value received so far (qty_received × PO price).
    pub received_cents: Cents,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiveLineInput {
    pub po_line_id: i64,
    pub qty: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoodsReceiptLineView {
    pub id: i64,
    pub po_line_id: i64,
    pub item_id: i64,
    pub sku: String,
    pub qty: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoodsReceiptView {
    pub id: i64,
    pub po_id: i64,
    /// The receipt's inventory entry (`None` for a zero-value receipt,
    /// which posts nothing).
    pub gl_entry_id: Option<i64>,
    pub lines: Vec<GoodsReceiptLineView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApLineInput {
    pub po_line_id: i64,
    pub qty: i64,
    pub unit_price_cents: Cents,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewApInvoice {
    pub po_id: i64,
    pub lines: Vec<ApLineInput>,
    /// The supplier's own invoice number, when known. This is the
    /// duplicate-detection key (audit fix, Oct 31): the same ref
    /// against the same PO is refused as a duplicate recording.
    /// Without a ref the system cannot distinguish a retry from a
    /// genuinely separate identical invoice, and recording proceeds.
    #[serde(default)]
    pub supplier_ref: Option<String>,
}

/// One three-way-match variance: which PO line, which kind
/// ("quantity" | "price"), and the plain-language detail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchVariance {
    pub po_line_id: i64,
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApInvoiceLineView {
    pub id: i64,
    pub po_line_id: i64,
    pub item_id: i64,
    pub sku: String,
    pub qty: i64,
    pub unit_price_cents: Cents,
    pub line_total_cents: Cents,
    /// The PO's agreed price for this line (what GR-IR clears at).
    pub po_unit_price_cents: Cents,
    /// Quantity received against the PO line so far.
    pub qty_received: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApInvoiceView {
    pub id: i64,
    pub po_id: i64,
    pub supplier_id: i64,
    pub supplier_name: String,
    pub status: String,
    /// "matched" | "variance" | "overridden" — as recorded; the live
    /// `variances` list is recomputed on every read.
    pub match_status: String,
    pub variances: Vec<MatchVariance>,
    pub override_reason: Option<String>,
    pub overridden_by: Option<String>,
    /// The supplier's own invoice number, when one was recorded.
    pub supplier_ref: Option<String>,
    pub lines: Vec<ApInvoiceLineView>,
    pub total_cents: Cents,
    pub paid_cents: Cents,
    pub balance_cents: Cents,
    pub gl_entry_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApPaymentView {
    pub id: i64,
    pub ap_invoice_id: i64,
    pub amount_cents: Cents,
    pub status: String,
    pub gl_entry_id: Option<i64>,
    /// The second approver when the payment approval rule fired
    /// (controls.rs); `None` for ordinary payments.
    pub approved_by: Option<String>,
}

// ---------------------------------------------------------------------------
// Suppliers
// ---------------------------------------------------------------------------

pub fn create_supplier(conn: &Connection, name: &str) -> Result<i64> {
    let name = name.trim();
    if name.is_empty() {
        return Err(LedgerError::Invalid("a supplier needs a name".to_string()));
    }
    let existing: i64 = conn.query_row(
        "SELECT COUNT(*) FROM suppliers WHERE name = ?1",
        params![name],
        |r| r.get(0),
    )?;
    if existing > 0 {
        return Err(LedgerError::Invalid(format!(
            "a supplier named '{name}' already exists"
        )));
    }
    conn.execute("INSERT INTO suppliers (name) VALUES (?1)", params![name])?;
    Ok(conn.last_insert_rowid())
}

pub fn list_suppliers(conn: &Connection) -> Result<Vec<SupplierView>> {
    let mut stmt = conn.prepare("SELECT id, name FROM suppliers ORDER BY id")?;
    let rows = stmt.query_map([], |row| {
        Ok(SupplierView {
            id: row.get(0)?,
            name: row.get(1)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn supplier_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM suppliers", [], |r| r.get(0))?)
}

// ---------------------------------------------------------------------------
// Requisitions
// ---------------------------------------------------------------------------

fn fetch_requisition(conn: &Connection, requisition_id: i64) -> Result<RequisitionView> {
    let (supplier_id, requested_by, status) = conn
        .query_row(
            "SELECT supplier_id, requested_by, status
             FROM purchase_requisitions WHERE id = ?1",
            params![requisition_id],
            |r| {
                Ok((
                    r.get::<_, Option<i64>>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(missing("purchase requisition", requisition_id))?;
    let supplier_name = match supplier_id {
        Some(id) => Some(supplier_name(conn, id)?),
        None => None,
    };
    let mut stmt = conn.prepare(
        "SELECT l.id, l.item_id, i.sku, l.qty, l.unit_price_cents
         FROM purchase_requisition_lines l
         JOIN items i ON i.id = l.item_id
         WHERE l.requisition_id = ?1 ORDER BY l.id",
    )?;
    let rows = stmt.query_map(params![requisition_id], |row| {
        let qty: i64 = row.get(3)?;
        let unit_price_cents: Cents = row.get(4)?;
        Ok(ReqLineView {
            id: row.get(0)?,
            item_id: row.get(1)?,
            sku: row.get(2)?,
            qty,
            unit_price_cents,
            line_total_cents: 0, // computed checked, below the row closure
        })
    })?;
    let mut lines = Vec::new();
    for r in rows {
        let mut l = r?;
        l.line_total_cents = mul_cents(l.qty, l.unit_price_cents, "a requisition line total")?;
        lines.push(l);
    }
    let total_cents = sum_cents(lines.iter().map(|l| l.line_total_cents), "a requisition total")?;
    Ok(RequisitionView {
        id: requisition_id,
        supplier_id,
        supplier_name,
        requested_by,
        status,
        lines,
        total_cents,
    })
}

pub fn get_requisition(conn: &Connection, requisition_id: i64) -> Result<Option<RequisitionView>> {
    match fetch_requisition(conn, requisition_id) {
        Ok(v) => Ok(Some(v)),
        Err(LedgerError::Invalid(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn list_requisitions(conn: &Connection) -> Result<Vec<RequisitionView>> {
    let mut stmt = conn.prepare("SELECT id FROM purchase_requisitions ORDER BY id")?;
    let ids = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for id in ids {
        out.push(fetch_requisition(conn, id?)?);
    }
    Ok(out)
}

fn validate_lines(conn: &Connection, lines: &[(i64, i64, Cents)], what: &str) -> Result<()> {
    if lines.is_empty() {
        return Err(LedgerError::Invalid(format!(
            "a {what} needs at least one line"
        )));
    }
    for (item_id, qty, price) in lines {
        if *qty <= 0 {
            return Err(LedgerError::Invalid(format!(
                "{what} quantities must be positive"
            )));
        }
        if *price < 0 {
            return Err(LedgerError::Invalid(format!(
                "{what} prices may not be negative"
            )));
        }
        item_sku(conn, *item_id)?;
    }
    Ok(())
}

pub fn create_requisition(conn: &Connection, new: &NewRequisition) -> Result<i64> {
    if let Some(supplier_id) = new.supplier_id {
        supplier_name(conn, supplier_id)?;
    }
    let raw: Vec<(i64, i64, Cents)> = new
        .lines
        .iter()
        .map(|l| (l.item_id, l.qty, l.unit_price_cents))
        .collect();
    validate_lines(conn, &raw, "purchase requisition")?;
    conn.execute(
        "INSERT INTO purchase_requisitions (supplier_id, requested_by, status)
         VALUES (?1, ?2, 'draft')",
        params![new.supplier_id, new.requested_by],
    )?;
    let requisition_id = conn.last_insert_rowid();
    for line in &new.lines {
        conn.execute(
            "INSERT INTO purchase_requisition_lines
                (requisition_id, item_id, qty, unit_price_cents)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                requisition_id,
                line.item_id,
                line.qty,
                line.unit_price_cents
            ],
        )?;
    }
    Ok(requisition_id)
}

/// One legal requisition state-machine step (local mirror of the
/// O2C `transition` helper's contract and wording).
fn requisition_transition(
    conn: &Connection,
    requisition_id: i64,
    from: &[&str],
    to: &str,
    action: &str,
    hint: &str,
) -> Result<()> {
    let status: String = conn
        .query_row(
            "SELECT status FROM purchase_requisitions WHERE id = ?1",
            params![requisition_id],
            |r| r.get(0),
        )
        .map_err(missing("purchase requisition", requisition_id))?;
    if !from.contains(&status.as_str()) {
        return Err(LedgerError::Invalid(format!(
            "cannot {action} purchase requisition {requisition_id}: it is '{status}' — {hint}"
        )));
    }
    conn.execute(
        "UPDATE purchase_requisitions SET status = ?1 WHERE id = ?2",
        params![to, requisition_id],
    )?;
    Ok(())
}

pub fn approve_requisition(conn: &Connection, requisition_id: i64) -> Result<()> {
    requisition_transition(
        conn,
        requisition_id,
        &["draft"],
        "approved",
        "approve",
        "only a draft requisition can be approved",
    )
}

pub fn reject_requisition(conn: &Connection, requisition_id: i64) -> Result<()> {
    requisition_transition(
        conn,
        requisition_id,
        &["draft"],
        "rejected",
        "reject",
        "only a draft requisition can be rejected",
    )
}

pub fn cancel_requisition(conn: &Connection, requisition_id: i64) -> Result<()> {
    requisition_transition(
        conn,
        requisition_id,
        &["draft", "approved"],
        "cancelled",
        "cancel",
        "a converted requisition cannot be cancelled — cancel its purchase order instead",
    )
}

// ---------------------------------------------------------------------------
// Procurement orders
// ---------------------------------------------------------------------------

fn fetch_proc_order(conn: &Connection, po_id: i64) -> Result<ProcOrderView> {
    let (supplier_id, requisition_id, status) = conn
        .query_row(
            "SELECT supplier_id, requisition_id, status
             FROM procurement_orders WHERE id = ?1",
            params![po_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(missing("procurement order", po_id))?;
    let mut stmt = conn.prepare(
        "SELECT l.id, l.item_id, i.sku, l.qty, l.unit_price_cents, l.qty_received
         FROM procurement_lines l
         JOIN items i ON i.id = l.item_id
         WHERE l.po_id = ?1 ORDER BY l.id",
    )?;
    let rows = stmt.query_map(params![po_id], |row| {
        let qty: i64 = row.get(3)?;
        let unit_price_cents: Cents = row.get(4)?;
        Ok(ProcOrderLineView {
            id: row.get(0)?,
            item_id: row.get(1)?,
            sku: row.get(2)?,
            qty,
            unit_price_cents,
            line_total_cents: 0, // computed checked, below the row closure
            qty_received: row.get(5)?,
        })
    })?;
    let mut lines = Vec::new();
    for r in rows {
        let mut l = r?;
        l.line_total_cents = mul_cents(l.qty, l.unit_price_cents, "a PO line total")?;
        lines.push(l);
    }
    let total_cents = sum_cents(lines.iter().map(|l| l.line_total_cents), "a PO total")?;
    let mut received_cents: Cents = 0;
    for l in &lines {
        received_cents = add_cents(
            received_cents,
            mul_cents(l.qty_received, l.unit_price_cents, "received value")?,
            "received value",
        )?;
    }
    Ok(ProcOrderView {
        id: po_id,
        supplier_id,
        supplier_name: supplier_name(conn, supplier_id)?,
        requisition_id,
        status,
        lines,
        total_cents,
        received_cents,
    })
}

pub fn get_procurement_order(conn: &Connection, po_id: i64) -> Result<Option<ProcOrderView>> {
    match fetch_proc_order(conn, po_id) {
        Ok(v) => Ok(Some(v)),
        Err(LedgerError::Invalid(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn list_procurement_orders(conn: &Connection) -> Result<Vec<ProcOrderView>> {
    let mut stmt = conn.prepare("SELECT id FROM procurement_orders ORDER BY id")?;
    let ids = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for id in ids {
        out.push(fetch_proc_order(conn, id?)?);
    }
    Ok(out)
}

pub fn create_purchase_order(conn: &mut Connection, new: &NewProcurementOrder) -> Result<i64> {
    let tx = conn.transaction()?;
    let mut supplier_id = new.supplier_id;
    let mut lines: Vec<ProcLineInput> = new.lines.clone();
    if let Some(requisition_id) = new.requisition_id {
        let req = fetch_requisition(&tx, requisition_id)?;
        if req.status != "approved" {
            return Err(LedgerError::Invalid(format!(
                "cannot create a purchase order from requisition {requisition_id}: \
                 it is '{}' — only an approved requisition converts",
                req.status
            )));
        }
        if supplier_id.is_none() {
            supplier_id = req.supplier_id;
        }
        if lines.is_empty() {
            lines = req
                .lines
                .iter()
                .map(|l| ProcLineInput {
                    item_id: l.item_id,
                    qty: l.qty,
                    unit_price_cents: l.unit_price_cents,
                })
                .collect();
        }
    }
    let supplier_id = supplier_id.ok_or_else(|| {
        LedgerError::Invalid(
            "a purchase order needs a supplier (directly, or from its requisition)".to_string(),
        )
    })?;
    supplier_name(&tx, supplier_id)?;
    let raw: Vec<(i64, i64, Cents)> = lines
        .iter()
        .map(|l| (l.item_id, l.qty, l.unit_price_cents))
        .collect();
    validate_lines(&tx, &raw, "purchase order")?;
    tx.execute(
        "INSERT INTO procurement_orders (supplier_id, requisition_id, status)
         VALUES (?1, ?2, 'open')",
        params![supplier_id, new.requisition_id],
    )?;
    let po_id = tx.last_insert_rowid();
    for line in &lines {
        tx.execute(
            "INSERT INTO procurement_lines (po_id, item_id, qty, unit_price_cents)
             VALUES (?1, ?2, ?3, ?4)",
            params![po_id, line.item_id, line.qty, line.unit_price_cents],
        )?;
    }
    if let Some(requisition_id) = new.requisition_id {
        tx.execute(
            "UPDATE purchase_requisitions SET status = 'converted' WHERE id = ?1",
            params![requisition_id],
        )?;
    }
    tx.commit()?;
    Ok(po_id)
}

pub fn cancel_purchase_order(conn: &Connection, po_id: i64) -> Result<()> {
    let order = fetch_proc_order(conn, po_id)?;
    if order.status != "open" {
        return Err(LedgerError::Invalid(format!(
            "cannot cancel procurement order {po_id}: it is '{}' — only an open \
             order with nothing received can be cancelled",
            order.status
        )));
    }
    if order.lines.iter().any(|l| l.qty_received > 0) {
        return Err(LedgerError::Invalid(format!(
            "cannot cancel procurement order {po_id}: goods have already been received against it"
        )));
    }
    conn.execute(
        "UPDATE procurement_orders SET status = 'cancelled' WHERE id = ?1",
        params![po_id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Goods receipts
// ---------------------------------------------------------------------------

fn fetch_receipt(conn: &Connection, receipt_id: i64) -> Result<GoodsReceiptView> {
    let (po_id, gl_entry_id) = conn
        .query_row(
            "SELECT po_id, gl_entry_id FROM goods_receipts WHERE id = ?1",
            params![receipt_id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)),
        )
        .map_err(missing("goods receipt", receipt_id))?;
    let mut stmt = conn.prepare(
        "SELECT l.id, l.po_line_id, l.item_id, i.sku, l.qty
         FROM goods_receipt_lines l
         JOIN items i ON i.id = l.item_id
         WHERE l.receipt_id = ?1 ORDER BY l.id",
    )?;
    let rows = stmt.query_map(params![receipt_id], |row| {
        Ok(GoodsReceiptLineView {
            id: row.get(0)?,
            po_line_id: row.get(1)?,
            item_id: row.get(2)?,
            sku: row.get(3)?,
            qty: row.get(4)?,
        })
    })?;
    let mut lines = Vec::new();
    for r in rows {
        lines.push(r?);
    }
    Ok(GoodsReceiptView {
        id: receipt_id,
        po_id,
        gl_entry_id,
        lines,
    })
}

pub fn list_goods_receipts(conn: &Connection, po_id: Option<i64>) -> Result<Vec<GoodsReceiptView>> {
    let mut stmt =
        conn.prepare("SELECT id FROM goods_receipts WHERE (?1 IS NULL OR po_id = ?1) ORDER BY id")?;
    let ids = stmt.query_map(params![po_id], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for id in ids {
        out.push(fetch_receipt(conn, id?)?);
    }
    Ok(out)
}

/// Receive goods against a procurement order. Quantities may arrive
/// in several receipts; receiving more than was ordered (cumulatively)
/// is refused. The receipt posts Dr inventory / Cr GR-IR at PO prices
/// in the same transaction, moves `stock_levels.on_hand`, advances
/// the order to `partially_received` / `received`, and re-evaluates
/// any on-hold AP invoices for the order — a quantity hold clears
/// itself when the goods it was waiting for arrive.
pub fn receive_goods(conn: &mut Connection, po_id: i64, lines: &[ReceiveLineInput]) -> Result<i64> {
    if lines.is_empty() {
        return Err(LedgerError::Invalid(
            "a goods receipt needs at least one line".to_string(),
        ));
    }
    let mut wanted: BTreeMap<i64, i64> = BTreeMap::new();
    for line in lines {
        if line.qty <= 0 {
            return Err(LedgerError::Invalid(
                "goods receipt quantities must be positive".to_string(),
            ));
        }
        *wanted.entry(line.po_line_id).or_default() += line.qty;
    }

    let tx = conn.transaction()?;
    ensure_procurement_accounts(&tx)?;
    let order = fetch_proc_order(&tx, po_id)?;
    if order.status != "open" && order.status != "partially_received" {
        return Err(LedgerError::Invalid(format!(
            "cannot receive goods against procurement order {po_id}: it is '{}'",
            order.status
        )));
    }
    // Validate every line against the order before writing anything.
    let mut debit_by_account: BTreeMap<&'static str, Cents> = BTreeMap::new();
    let mut total_cents: Cents = 0;
    for (po_line_id, qty) in &wanted {
        let line = order
            .lines
            .iter()
            .find(|l| l.id == *po_line_id)
            .ok_or_else(|| {
                LedgerError::Invalid(format!(
                    "purchase order line {po_line_id} does not belong to procurement order {po_id}"
                ))
            })?;
        if line.qty_received + qty > line.qty {
            return Err(LedgerError::Invalid(format!(
                "receiving {qty} more of {} would exceed the ordered quantity {} \
                 ({} already received)",
                line.sku, line.qty, line.qty_received
            )));
        }
        let make_or_buy: String = tx.query_row(
            "SELECT make_or_buy FROM items WHERE id = ?1",
            params![line.item_id],
            |r| r.get(0),
        )?;
        let account = if make_or_buy == "make" {
            ACCT_FINISHED_GOODS
        } else {
            ACCT_RAW_MATERIALS
        };
        let received_value = mul_cents(*qty, line.unit_price_cents, "a goods-receipt line")?;
        let slot = debit_by_account.entry(account).or_default();
        *slot = add_cents(*slot, received_value, "goods-receipt debits")?;
        total_cents = add_cents(total_cents, received_value, "a goods-receipt total")?;
    }

    tx.execute(
        "INSERT INTO goods_receipts (po_id) VALUES (?1)",
        params![po_id],
    )?;
    let receipt_id = tx.last_insert_rowid();
    for (po_line_id, qty) in &wanted {
        let line = order.lines.iter().find(|l| l.id == *po_line_id).unwrap();
        tx.execute(
            "INSERT INTO goods_receipt_lines (receipt_id, po_line_id, item_id, qty)
             VALUES (?1, ?2, ?3, ?4)",
            params![receipt_id, po_line_id, line.item_id, qty],
        )?;
        tx.execute(
            "UPDATE procurement_lines SET qty_received = qty_received + ?1 WHERE id = ?2",
            params![qty, po_line_id],
        )?;
        tx.execute(
            "INSERT INTO stock_levels (item_id, on_hand, on_order, wip)
             VALUES (?1, ?2, 0, 0)
             ON CONFLICT(item_id) DO UPDATE SET on_hand = on_hand + ?2",
            params![line.item_id, qty],
        )?;
    }

    // A zero-value receipt (free goods) posts nothing — the same
    // "documents that move no value post no entry" rule as O2C quotes.
    if total_cents > 0 {
        let created_at: String = tx.query_row(
            "SELECT created_at FROM goods_receipts WHERE id = ?1",
            params![receipt_id],
            |r| r.get(0),
        )?;
        let mut entry_lines: Vec<NewLine> = debit_by_account
            .iter()
            .filter(|(_, amount)| **amount > 0)
            .map(|(account, amount)| NewLine {
                account_code: account.to_string(),
                debit_cents: *amount,
                credit_cents: 0,
            })
            .collect();
        entry_lines.push(NewLine {
            account_code: ACCT_GRIR.to_string(),
            debit_cents: 0,
            credit_cents: total_cents,
        });
        let gl_entry_id = post_entry_in(
            &tx,
            &NewEntry {
                entry_date: date_part(&created_at),
                memo: format!(
                    "Goods receipt {receipt_id} on procurement order {po_id} ({})",
                    order.supplier_name
                ),
                source: format!("goods_receipt:{receipt_id}"),
                reverses_entry_id: None,
                lines: entry_lines,
            },
        )?;
        tx.execute(
            "UPDATE goods_receipts SET gl_entry_id = ?1 WHERE id = ?2",
            params![gl_entry_id, receipt_id],
        )?;
    }

    let fully_received = order
        .lines
        .iter()
        .all(|l| l.qty_received + wanted.get(&l.id).copied().unwrap_or(0) >= l.qty);
    tx.execute(
        "UPDATE procurement_orders SET status = ?1 WHERE id = ?2",
        params![
            if fully_received {
                "received"
            } else {
                "partially_received"
            },
            po_id
        ],
    )?;

    // Arriving goods can clear a quantity hold on this order's AP
    // invoices: re-run their matches and release the ones now clean.
    let held: Vec<i64> = {
        let mut stmt =
            tx.prepare("SELECT id FROM ap_invoices WHERE po_id = ?1 AND status = 'on_hold'")?;
        let rows = stmt.query_map(params![po_id], |r| r.get::<_, i64>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        out
    };
    for ap_invoice_id in held {
        if match_variances(&tx, ap_invoice_id)?.is_empty() {
            tx.execute(
                "UPDATE ap_invoices SET status = 'open', match_status = 'matched'
                 WHERE id = ?1",
                params![ap_invoice_id],
            )?;
        }
    }
    tx.commit()?;
    Ok(receipt_id)
}

// ---------------------------------------------------------------------------
// AP invoices and the three-way match
// ---------------------------------------------------------------------------

/// The live three-way match for one AP invoice: for every invoiced
/// line, the cumulative invoiced quantity vs. what has been received
/// (quantity variance), and the invoiced price vs. the PO price
/// (price variance). Computed from current state on every call —
/// the match is a fact about three documents, never a stored guess.
fn match_variances(conn: &Connection, ap_invoice_id: i64) -> Result<Vec<MatchVariance>> {
    let mut stmt = conn.prepare(
        "SELECT l.po_line_id, i.sku, l.qty, l.unit_price_cents,
                p.qty, p.unit_price_cents, p.qty_received,
                (SELECT COALESCE(SUM(l2.qty), 0) FROM ap_invoice_lines l2
                  WHERE l2.po_line_id = l.po_line_id)
         FROM ap_invoice_lines l
         JOIN procurement_lines p ON p.id = l.po_line_id
         JOIN items i ON i.id = l.item_id
         WHERE l.ap_invoice_id = ?1
         ORDER BY l.id",
    )?;
    let rows = stmt.query_map(params![ap_invoice_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Cents>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Cents>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, i64>(7)?,
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (po_line_id, sku, _line_qty, inv_price, _po_qty, po_price, received, cum_invoiced) = r?;
        if cum_invoiced > received {
            out.push(MatchVariance {
                po_line_id,
                kind: "quantity".to_string(),
                message: format!(
                    "{sku}: {cum_invoiced} invoiced against purchase order line \
                     {po_line_id} but only {received} received"
                ),
            });
        }
        if inv_price != po_price {
            out.push(MatchVariance {
                po_line_id,
                kind: "price".to_string(),
                message: format!(
                    "{sku}: invoiced at {inv_price} cents vs. the purchase order \
                     price of {po_price} cents (line {po_line_id})"
                ),
            });
        }
    }
    Ok(out)
}

fn fetch_ap_invoice(conn: &Connection, ap_invoice_id: i64) -> Result<ApInvoiceView> {
    let (
        po_id,
        supplier_id,
        status,
        match_status,
        override_reason,
        overridden_by,
        gl_entry_id,
        supplier_ref,
    ) = conn.query_row(
            "SELECT po_id, supplier_id, status, match_status, override_reason,
                    overridden_by, gl_entry_id, supplier_ref
             FROM ap_invoices WHERE id = ?1",
            params![ap_invoice_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                ))
            },
        )
        .map_err(missing("AP invoice", ap_invoice_id))?;
    let mut stmt = conn.prepare(
        "SELECT l.id, l.po_line_id, l.item_id, i.sku, l.qty, l.unit_price_cents,
                p.unit_price_cents, p.qty_received
         FROM ap_invoice_lines l
         JOIN procurement_lines p ON p.id = l.po_line_id
         JOIN items i ON i.id = l.item_id
         WHERE l.ap_invoice_id = ?1 ORDER BY l.id",
    )?;
    let rows = stmt.query_map(params![ap_invoice_id], |row| {
        let qty: i64 = row.get(4)?;
        let unit_price_cents: Cents = row.get(5)?;
        Ok(ApInvoiceLineView {
            id: row.get(0)?,
            po_line_id: row.get(1)?,
            item_id: row.get(2)?,
            sku: row.get(3)?,
            qty,
            unit_price_cents,
            line_total_cents: 0, // computed checked, below the row closure
            po_unit_price_cents: row.get(6)?,
            qty_received: row.get(7)?,
        })
    })?;
    let mut lines = Vec::new();
    for r in rows {
        let mut l = r?;
        l.line_total_cents = mul_cents(l.qty, l.unit_price_cents, "an AP invoice line total")?;
        lines.push(l);
    }
    let total_cents = sum_cents(lines.iter().map(|l| l.line_total_cents), "an AP invoice total")?;
    let paid_cents: Cents = conn.query_row(
        "SELECT COALESCE(SUM(amount_cents), 0) FROM ap_payments WHERE ap_invoice_id = ?1",
        params![ap_invoice_id],
        |r| r.get(0),
    )?;
    Ok(ApInvoiceView {
        id: ap_invoice_id,
        po_id,
        supplier_id,
        supplier_name: supplier_name(conn, supplier_id)?,
        status,
        match_status,
        variances: match_variances(conn, ap_invoice_id)?,
        override_reason,
        overridden_by,
        supplier_ref,
        lines,
        total_cents,
        paid_cents,
        balance_cents: total_cents - paid_cents,
        gl_entry_id,
    })
}

pub fn get_ap_invoice(conn: &Connection, ap_invoice_id: i64) -> Result<Option<ApInvoiceView>> {
    match fetch_ap_invoice(conn, ap_invoice_id) {
        Ok(v) => Ok(Some(v)),
        Err(LedgerError::Invalid(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn list_ap_invoices(conn: &Connection) -> Result<Vec<ApInvoiceView>> {
    let mut stmt = conn.prepare("SELECT id FROM ap_invoices ORDER BY id")?;
    let ids = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for id in ids {
        out.push(fetch_ap_invoice(conn, id?)?);
    }
    Ok(out)
}

pub fn ap_invoice_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM ap_invoices", [], |r| r.get(0))?)
}

/// Record a supplier's invoice against a procurement order and post
/// it: Dr GR-IR at PO prices, the price difference to PPV, Cr AP at
/// the invoice total — in the same transaction as the document. The
/// three-way match runs immediately: a clean invoice lands `open`;
/// any variance lands it `on_hold`, recorded but not payable.
pub fn record_ap_invoice(conn: &mut Connection, new: &NewApInvoice) -> Result<i64> {
    // Duplicate guard (audit fix, Oct 31): with a supplier reference,
    // the same invoice against the same PO records exactly once. A
    // retried submission used to match and pay a second time.
    let supplier_ref = new
        .supplier_ref
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty());
    if let Some(r) = supplier_ref {
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM ap_invoices WHERE po_id = ?1 AND supplier_ref = ?2",
                params![new.po_id, r],
                |row| row.get(0),
            )
            .ok();
        if let Some(id) = existing {
            return Err(LedgerError::Invalid(format!(
                "supplier invoice '{r}' is already recorded against purchase order {} \
                 as AP invoice #{id} — refusing to record the same supplier invoice twice",
                new.po_id
            )));
        }
    }
    if new.lines.is_empty() {
        return Err(LedgerError::Invalid(
            "an AP invoice needs at least one line".to_string(),
        ));
    }
    let mut wanted: BTreeMap<i64, (i64, Cents)> = BTreeMap::new();
    for line in &new.lines {
        if line.qty <= 0 {
            return Err(LedgerError::Invalid(
                "AP invoice quantities must be positive".to_string(),
            ));
        }
        if line.unit_price_cents < 0 {
            return Err(LedgerError::Invalid(
                "AP invoice prices may not be negative".to_string(),
            ));
        }
        let entry = wanted
            .entry(line.po_line_id)
            .or_insert((0, line.unit_price_cents));
        if entry.1 != line.unit_price_cents {
            return Err(LedgerError::Invalid(format!(
                "AP invoice line for purchase order line {} appears twice at \
                 different prices — send one line per purchase order line",
                line.po_line_id
            )));
        }
        entry.0 += line.qty;
    }

    let tx = conn.transaction()?;
    ensure_procurement_accounts(&tx)?;
    let order = fetch_proc_order(&tx, new.po_id)?;
    if order.status == "cancelled" {
        return Err(LedgerError::Invalid(format!(
            "cannot record an AP invoice against procurement order {}: it is cancelled",
            new.po_id
        )));
    }
    let mut grir_cents: Cents = 0;
    let mut total_cents: Cents = 0;
    for (po_line_id, (qty, inv_price)) in &wanted {
        let line = order
            .lines
            .iter()
            .find(|l| l.id == *po_line_id)
            .ok_or_else(|| {
                LedgerError::Invalid(format!(
                    "purchase order line {po_line_id} does not belong to \
                     procurement order {}",
                    new.po_id
                ))
            })?;
        let already_invoiced: i64 = tx.query_row(
            "SELECT COALESCE(SUM(qty), 0) FROM ap_invoice_lines WHERE po_line_id = ?1",
            params![po_line_id],
            |r| r.get(0),
        )?;
        if already_invoiced + qty > line.qty {
            return Err(LedgerError::Invalid(format!(
                "invoicing {qty} more of {} would exceed the ordered quantity {} \
                 ({already_invoiced} already invoiced) — that is a billing error, \
                 not a match variance",
                line.sku, line.qty
            )));
        }
        grir_cents = add_cents(
            grir_cents,
            mul_cents(*qty, line.unit_price_cents, "GR-IR clearing")?,
            "GR-IR clearing",
        )?;
        total_cents = add_cents(
            total_cents,
            mul_cents(*qty, *inv_price, "an AP invoice line")?,
            "an AP invoice total",
        )?;
    }
    if total_cents <= 0 {
        return Err(LedgerError::Invalid(
            "cannot record an AP invoice with a zero total — there is no payable to recognize"
                .to_string(),
        ));
    }

    tx.execute(
        "INSERT INTO ap_invoices (po_id, supplier_id, status, match_status, supplier_ref)
         VALUES (?1, ?2, 'open', 'matched', ?3)",
        params![new.po_id, order.supplier_id, supplier_ref],
    )?;
    let ap_invoice_id = tx.last_insert_rowid();
    for (po_line_id, (qty, inv_price)) in &wanted {
        let line = order.lines.iter().find(|l| l.id == *po_line_id).unwrap();
        tx.execute(
            "INSERT INTO ap_invoice_lines (ap_invoice_id, po_line_id, item_id, qty, unit_price_cents)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ap_invoice_id, po_line_id, line.item_id, qty, inv_price],
        )?;
    }

    let ppv_cents = total_cents - grir_cents;
    let created_at: String = tx.query_row(
        "SELECT created_at FROM ap_invoices WHERE id = ?1",
        params![ap_invoice_id],
        |r| r.get(0),
    )?;
    let mut entry_lines: Vec<NewLine> = Vec::new();
    if grir_cents > 0 {
        entry_lines.push(NewLine {
            account_code: ACCT_GRIR.to_string(),
            debit_cents: grir_cents,
            credit_cents: 0,
        });
    }
    if ppv_cents > 0 {
        entry_lines.push(NewLine {
            account_code: ACCT_PPV.to_string(),
            debit_cents: ppv_cents,
            credit_cents: 0,
        });
    } else if ppv_cents < 0 {
        entry_lines.push(NewLine {
            account_code: ACCT_PPV.to_string(),
            debit_cents: 0,
            credit_cents: -ppv_cents,
        });
    }
    entry_lines.push(NewLine {
        account_code: ACCT_AP.to_string(),
        debit_cents: 0,
        credit_cents: total_cents,
    });
    let gl_entry_id = post_entry_in(
        &tx,
        &NewEntry {
            entry_date: date_part(&created_at),
            memo: format!(
                "AP invoice {ap_invoice_id} recorded — {} (procurement order {})",
                order.supplier_name, new.po_id
            ),
            source: format!("ap_invoice:{ap_invoice_id}"),
            reverses_entry_id: None,
            lines: entry_lines,
        },
    )?;
    tx.execute(
        "UPDATE ap_invoices SET gl_entry_id = ?1 WHERE id = ?2",
        params![gl_entry_id, ap_invoice_id],
    )?;

    let variances = match_variances(&tx, ap_invoice_id)?;
    if !variances.is_empty() {
        tx.execute(
            "UPDATE ap_invoices SET status = 'on_hold', match_status = 'variance'
             WHERE id = ?1",
            params![ap_invoice_id],
        )?;
    }
    tx.commit()?;
    Ok(ap_invoice_id)
}

/// Override a three-way-match hold with a recorded reason. The
/// override is itself part of the document's record — reason and
/// approver live on the AP invoice header, in every view of it —
/// because that is what an auditor asks for first: who let this
/// payment through, and why. Only an on-hold invoice can be
/// overridden; the match facts (the live variance list) do not
/// change — the override records that a human accepted them.
pub fn override_ap_match(
    conn: &Connection,
    ap_invoice_id: i64,
    reason: &str,
    overridden_by: &str,
) -> Result<()> {
    let reason = reason.trim();
    let overridden_by = overridden_by.trim();
    if reason.is_empty() {
        return Err(LedgerError::Invalid(
            "a match override needs a reason — it is recorded on the invoice".to_string(),
        ));
    }
    if overridden_by.is_empty() {
        return Err(LedgerError::Invalid(
            "a match override needs the name of the person overriding".to_string(),
        ));
    }
    let view = fetch_ap_invoice(conn, ap_invoice_id)?;
    if view.status != "on_hold" {
        return Err(LedgerError::Invalid(format!(
            "cannot override the match on AP invoice {ap_invoice_id}: it is '{}' — \
             only an on-hold invoice has a match to override",
            view.status
        )));
    }
    conn.execute(
        "UPDATE ap_invoices
         SET status = 'open', match_status = 'overridden',
             override_reason = ?1, overridden_by = ?2
         WHERE id = ?3",
        params![reason, overridden_by, ap_invoice_id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// AP payments
// ---------------------------------------------------------------------------

fn fetch_ap_payment(conn: &Connection, payment_id: i64) -> Result<ApPaymentView> {
    conn.query_row(
        "SELECT id, ap_invoice_id, amount_cents, status, gl_entry_id, approved_by
         FROM ap_payments WHERE id = ?1",
        params![payment_id],
        |r| {
            Ok(ApPaymentView {
                id: r.get(0)?,
                ap_invoice_id: r.get(1)?,
                amount_cents: r.get(2)?,
                status: r.get(3)?,
                gl_entry_id: r.get(4)?,
                approved_by: r.get(5)?,
            })
        },
    )
    .map_err(missing("AP payment", payment_id))
}

pub fn list_ap_payments(conn: &Connection) -> Result<Vec<ApPaymentView>> {
    let mut stmt = conn.prepare("SELECT id FROM ap_payments ORDER BY id")?;
    let ids = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for id in ids {
        out.push(fetch_ap_payment(conn, id?)?);
    }
    Ok(out)
}

pub fn list_ap_payments_for_invoice(
    conn: &Connection,
    ap_invoice_id: i64,
) -> Result<Vec<ApPaymentView>> {
    fetch_ap_invoice(conn, ap_invoice_id)?;
    let mut stmt =
        conn.prepare("SELECT id FROM ap_payments WHERE ap_invoice_id = ?1 ORDER BY id")?;
    let ids = stmt.query_map(params![ap_invoice_id], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for id in ids {
        out.push(fetch_ap_payment(conn, id?)?);
    }
    Ok(out)
}

/// Pay (part of) an open AP invoice: Dr AP / Cr Cash in the same
/// transaction, advancing the invoice toward `paid`. An on-hold
/// invoice refuses with its first variance; overpayment is refused,
/// never silently accepted.
pub fn pay_ap_invoice(
    conn: &mut Connection,
    ap_invoice_id: i64,
    amount_cents: Cents,
) -> Result<i64> {
    pay_ap_invoice_attributed(conn, ap_invoice_id, amount_cents, None, None)
}

/// [`pay_ap_invoice`] with actor attribution for the controls
/// approval rule (controls.rs): above the configured threshold, an
/// actor and a different approver (role approver/admin) are
/// required; the approver is recorded on the payment row and in the
/// posted entry's memo.
pub fn pay_ap_invoice_attributed(
    conn: &mut Connection,
    ap_invoice_id: i64,
    amount_cents: Cents,
    actor: Option<&str>,
    approver: Option<&str>,
) -> Result<i64> {
    if amount_cents <= 0 {
        return Err(LedgerError::Invalid(
            "an AP payment amount must be positive".to_string(),
        ));
    }
    let tx = conn.transaction()?;
    let view = fetch_ap_invoice(&tx, ap_invoice_id)?;
    if view.status == "on_hold" {
        let first = view
            .variances
            .first()
            .map(|v| v.message.clone())
            .unwrap_or_else(|| "a match variance".to_string());
        return Err(LedgerError::Invalid(format!(
            "cannot pay AP invoice {ap_invoice_id}: it is on three-way-match hold — \
             {first}. Resolve the variance or override the match with a reason"
        )));
    }
    if view.status != "open" && view.status != "partially_paid" {
        return Err(LedgerError::Invalid(format!(
            "cannot pay AP invoice {ap_invoice_id}: it is '{}' — payments are \
             accepted only on open invoices",
            view.status
        )));
    }
    if view.paid_cents + amount_cents > view.total_cents {
        return Err(LedgerError::Invalid(format!(
            "payment of {amount_cents} cents exceeds the balance of AP invoice \
             {ap_invoice_id}: it totals {} cents with {} cents already paid",
            view.total_cents, view.paid_cents
        )));
    }
    // Segregation of duties, same rule as AR payments (controls.rs).
    let approved_by =
        crate::controls::check_payment_approval(&tx, amount_cents, actor, approver, "AP payment")?;
    tx.execute(
        "INSERT INTO ap_payments (ap_invoice_id, amount_cents, status, approved_by)
         VALUES (?1, ?2, 'recorded', ?3)",
        params![ap_invoice_id, amount_cents, approved_by],
    )?;
    let payment_id = tx.last_insert_rowid();
    let created_at: String = tx.query_row(
        "SELECT created_at FROM ap_payments WHERE id = ?1",
        params![payment_id],
        |r| r.get(0),
    )?;
    let mut memo = format!(
        "AP payment {payment_id} — {} (AP invoice {ap_invoice_id})",
        view.supplier_name
    );
    if let Some(name) = &approved_by {
        memo.push_str(&format!(" — approved by {name}"));
    }
    let gl_entry_id = post_entry_in(
        &tx,
        &NewEntry {
            entry_date: date_part(&created_at),
            memo,
            source: format!("ap_payment:{payment_id}"),
            reverses_entry_id: None,
            lines: vec![
                NewLine {
                    account_code: ACCT_AP.to_string(),
                    debit_cents: amount_cents,
                    credit_cents: 0,
                },
                NewLine {
                    account_code: ACCT_CASH.to_string(),
                    debit_cents: 0,
                    credit_cents: amount_cents,
                },
            ],
        },
    )?;
    tx.execute(
        "UPDATE ap_payments SET gl_entry_id = ?1 WHERE id = ?2",
        params![gl_entry_id, payment_id],
    )?;
    let new_status = if view.paid_cents + amount_cents == view.total_cents {
        "paid"
    } else {
        "partially_paid"
    };
    tx.execute(
        "UPDATE ap_invoices SET status = ?1 WHERE id = ?2",
        params![new_status, ap_invoice_id],
    )?;
    tx.commit()?;
    Ok(payment_id)
}

/// Pre-flight for a multi-step agent action that ends in paying this
/// invoice (audit fix, Oct 31): everything the payment would refuse
/// for reasons knowable BEFORE any step runs — a missing, paid or
/// (without the sequence's own override) held invoice, an amount
/// above the collectible balance, a closed fiscal period for today.
/// The agent validates the whole sequence up front, so a refused
/// payment never leaves a match override behind it.
pub fn check_ap_payment_ready(
    conn: &Connection,
    ap_invoice_id: i64,
    amount_cents: Cents,
    match_overridden: bool,
) -> std::result::Result<(), String> {
    let invoice = fetch_ap_invoice(conn, ap_invoice_id).map_err(|e| e.to_string())?;
    match invoice.status.as_str() {
        "paid" => {
            return Err(format!("AP invoice #{ap_invoice_id} is already paid"));
        }
        "on_hold" if !match_overridden => {
            return Err(format!(
                "AP invoice #{ap_invoice_id} is on a three-way-match hold"
            ));
        }
        _ => {}
    }
    if amount_cents > invoice.balance_cents {
        return Err(format!(
            "payment of {amount_cents} cents exceeds the balance of AP invoice \
             #{ap_invoice_id} ({} cents)",
            invoice.balance_cents
        ));
    }
    let today: String = conn
        .query_row("SELECT date('now')", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if let Some(period) =
        crate::controls::closed_period_for(conn, &today).map_err(|e| e.to_string())?
    {
        return Err(format!(
            "cannot post a payment dated {today}: fiscal period '{period}' is closed"
        ));
    }
    Ok(())
}
