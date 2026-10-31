//! AI-native MRP — the Warp Speed pattern (see MRP_DESIGN.md).
//!
//! A plan is never stored: `run_mrp` computes from the state that
//! exists at the moment of the run (open sales orders, stock levels,
//! WIP, open supply, BOMs, and the configurator's component
//! structure), so a change to an order or a configuration is
//! reflected in the very next run — dynamic by construction.
//!
//! Every planned order is explainable: it carries the netting math
//! and the demand reasons behind it. A shortage is a planned order
//! whose start date is already past. Releasing a planned order
//! creates the production/purchase *document* and updates the
//! quantity state (WIP / on-order); no journal entry posts on
//! release — value moves only through the existing posting map
//! when real events happen (materials issued, production completed,
//! supplier paid).

use crate::configurator::Verdict;
use crate::rusqlite::{params, Connection};
use crate::{evaluate_configuration, list_orders, ConfigValue, LedgerError, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct StockLevelView {
    pub item_id: i64,
    pub sku: String,
    pub name: String,
    pub make_or_buy: String,
    pub lead_time_days: i64,
    pub on_hand: i64,
    pub on_order: i64,
    pub wip: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BomLineView {
    pub parent_item_id: i64,
    pub component_item_id: i64,
    pub component_sku: String,
    pub qty_per: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProductionOrderView {
    pub id: i64,
    pub item_id: i64,
    pub sku: String,
    pub qty: i64,
    pub due_date: String,
    pub start_date: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PurchaseOrderView {
    pub id: i64,
    pub item_id: i64,
    pub sku: String,
    pub qty: i64,
    pub due_date: String,
    pub status: String,
}

// ---------------------------------------------------------------------------
// Planning state: stock, BOM, item planning attributes
// ---------------------------------------------------------------------------

fn item_id_by_sku(conn: &Connection, sku: &str) -> Result<Option<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM items WHERE sku = ?1")?;
    let mut rows = stmt.query(params![sku])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

pub fn list_stock(conn: &Connection) -> Result<Vec<StockLevelView>> {
    let mut stmt = conn.prepare(
        "SELECT i.id, i.sku, i.name, i.make_or_buy, i.lead_time_days,
                COALESCE(s.on_hand, 0), COALESCE(s.on_order, 0), COALESCE(s.wip, 0)
         FROM items i
         LEFT JOIN stock_levels s ON s.item_id = i.id
         ORDER BY i.id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(StockLevelView {
            item_id: row.get(0)?,
            sku: row.get(1)?,
            name: row.get(2)?,
            make_or_buy: row.get(3)?,
            lead_time_days: row.get(4)?,
            on_hand: row.get(5)?,
            on_order: row.get(6)?,
            wip: row.get(7)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn stock_for(conn: &Connection, item_id: i64) -> Result<(i64, i64, i64)> {
    let mut stmt =
        conn.prepare("SELECT on_hand, on_order, wip FROM stock_levels WHERE item_id = ?1")?;
    let mut rows = stmt.query(params![item_id])?;
    match rows.next()? {
        Some(row) => Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        None => Ok((0, 0, 0)),
    }
}

fn write_stock(
    conn: &Connection,
    item_id: i64,
    on_hand: i64,
    on_order: i64,
    wip: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO stock_levels (item_id, on_hand, on_order, wip) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(item_id) DO UPDATE SET on_hand = ?2, on_order = ?3, wip = ?4",
        params![item_id, on_hand, on_order, wip],
    )?;
    Ok(())
}

/// Record the physical truth for one item's quantities.
pub fn set_stock(
    conn: &Connection,
    item_id: i64,
    on_hand: i64,
    on_order: i64,
    wip: i64,
) -> Result<StockLevelView> {
    if item_id_by_id(conn, item_id)?.is_none() {
        return Err(LedgerError::Invalid(format!("no item with id {item_id}")));
    }
    if on_hand < 0 || on_order < 0 || wip < 0 {
        return Err(LedgerError::Invalid(
            "stock quantities cannot be negative".to_string(),
        ));
    }
    write_stock(conn, item_id, on_hand, on_order, wip)?;
    list_stock(conn)?
        .into_iter()
        .find(|s| s.item_id == item_id)
        .ok_or_else(|| LedgerError::Invalid(format!("no item with id {item_id}")))
}

fn item_id_by_id(conn: &Connection, item_id: i64) -> Result<Option<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM items WHERE id = ?1")?;
    let mut rows = stmt.query(params![item_id])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

/// Set an item's planning attributes (lead time, make-or-buy).
pub fn set_item_planning(
    conn: &Connection,
    item_id: i64,
    lead_time_days: Option<i64>,
    make_or_buy: Option<&str>,
) -> Result<StockLevelView> {
    if item_id_by_id(conn, item_id)?.is_none() {
        return Err(LedgerError::Invalid(format!("no item with id {item_id}")));
    }
    if let Some(days) = lead_time_days {
        if days < 0 {
            return Err(LedgerError::Invalid(
                "lead time cannot be negative".to_string(),
            ));
        }
        conn.execute(
            "UPDATE items SET lead_time_days = ?2 WHERE id = ?1",
            params![item_id, days],
        )?;
    }
    if let Some(mb) = make_or_buy {
        if mb != "make" && mb != "buy" {
            return Err(LedgerError::Invalid(format!(
                "make_or_buy must be \"make\" or \"buy\", got \"{mb}\""
            )));
        }
        conn.execute(
            "UPDATE items SET make_or_buy = ?2 WHERE id = ?1",
            params![item_id, mb],
        )?;
    }
    list_stock(conn)?
        .into_iter()
        .find(|s| s.item_id == item_id)
        .ok_or_else(|| LedgerError::Invalid(format!("no item with id {item_id}")))
}

pub fn list_bom(conn: &Connection, parent_item_id: i64) -> Result<Vec<BomLineView>> {
    let mut stmt = conn.prepare(
        "SELECT b.component_item_id, i.sku, b.qty_per
         FROM bom_lines b JOIN items i ON i.id = b.component_item_id
         WHERE b.parent_item_id = ?1 ORDER BY b.id",
    )?;
    let rows = stmt.query_map(params![parent_item_id], |row| {
        Ok(BomLineView {
            parent_item_id,
            component_item_id: row.get(0)?,
            component_sku: row.get(1)?,
            qty_per: row.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn add_bom_line(
    conn: &Connection,
    parent_item_id: i64,
    component_item_id: i64,
    qty_per: i64,
) -> Result<BomLineView> {
    if qty_per <= 0 {
        return Err(LedgerError::Invalid(
            "a BOM line quantity must be positive".to_string(),
        ));
    }
    if parent_item_id == component_item_id {
        return Err(LedgerError::Invalid(
            "an item cannot be its own BOM component".to_string(),
        ));
    }
    for id in [parent_item_id, component_item_id] {
        if item_id_by_id(conn, id)?.is_none() {
            return Err(LedgerError::Invalid(format!("no item with id {id}")));
        }
    }
    let inserted = conn.execute(
        "INSERT INTO bom_lines (parent_item_id, component_item_id, qty_per)
         VALUES (?1, ?2, ?3) ON CONFLICT(parent_item_id, component_item_id) DO NOTHING",
        params![parent_item_id, component_item_id, qty_per],
    )?;
    if inserted == 0 {
        return Err(LedgerError::Invalid(format!(
            "item {component_item_id} is already on the BOM of item {parent_item_id}"
        )));
    }
    list_bom(conn, parent_item_id)?
        .into_iter()
        .find(|l| l.component_item_id == component_item_id)
        .ok_or_else(|| LedgerError::Invalid("BOM line vanished after insert".to_string()))
}

// ---------------------------------------------------------------------------
// Released documents
// ---------------------------------------------------------------------------

pub fn list_production_orders(conn: &Connection) -> Result<Vec<ProductionOrderView>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.item_id, i.sku, p.qty, p.due_date, p.start_date, p.status
         FROM production_orders p JOIN items i ON i.id = p.item_id ORDER BY p.id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(ProductionOrderView {
            id: row.get(0)?,
            item_id: row.get(1)?,
            sku: row.get(2)?,
            qty: row.get(3)?,
            due_date: row.get(4)?,
            start_date: row.get(5)?,
            status: row.get(6)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn list_purchase_orders(conn: &Connection) -> Result<Vec<PurchaseOrderView>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.item_id, i.sku, p.qty, p.due_date, p.status
         FROM purchase_orders p JOIN items i ON i.id = p.item_id ORDER BY p.id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(PurchaseOrderView {
            id: row.get(0)?,
            item_id: row.get(1)?,
            sku: row.get(2)?,
            qty: row.get(3)?,
            due_date: row.get(4)?,
            status: row.get(5)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// A purchase arrives: on-order becomes on-hand.
pub fn receive_purchase_order(
    conn: &Connection,
    purchase_order_id: i64,
) -> Result<PurchaseOrderView> {
    let order = list_purchase_orders(conn)?
        .into_iter()
        .find(|o| o.id == purchase_order_id)
        .ok_or_else(|| {
            LedgerError::Invalid(format!("no purchase order with id {purchase_order_id}"))
        })?;
    if order.status != "open" {
        return Err(LedgerError::Invalid(format!(
            "purchase order {purchase_order_id} is {} — only an open order can be received",
            order.status
        )));
    }
    let (on_hand, on_order, wip) = stock_for(conn, order.item_id)?;
    if on_order < order.qty {
        return Err(LedgerError::Invalid(format!(
            "purchase order {purchase_order_id} expects {} on order for {}, but only {on_order} \
             is recorded — the quantity state is inconsistent",
            order.qty, order.sku
        )));
    }
    write_stock(
        conn,
        order.item_id,
        on_hand + order.qty,
        on_order - order.qty,
        wip,
    )?;
    conn.execute(
        "UPDATE purchase_orders SET status = 'received' WHERE id = ?1",
        params![purchase_order_id],
    )?;
    list_purchase_orders(conn)?
        .into_iter()
        .find(|o| o.id == purchase_order_id)
        .ok_or_else(|| LedgerError::Invalid("purchase order vanished".to_string()))
}

/// Production finishes: WIP becomes on-hand finished stock.
pub fn complete_production_order(
    conn: &Connection,
    production_order_id: i64,
) -> Result<ProductionOrderView> {
    let order = list_production_orders(conn)?
        .into_iter()
        .find(|o| o.id == production_order_id)
        .ok_or_else(|| {
            LedgerError::Invalid(format!("no production order with id {production_order_id}"))
        })?;
    if order.status != "open" {
        return Err(LedgerError::Invalid(format!(
            "production order {production_order_id} is {} — only an open order can be completed",
            order.status
        )));
    }
    let (on_hand, on_order, wip) = stock_for(conn, order.item_id)?;
    if wip < order.qty {
        return Err(LedgerError::Invalid(format!(
            "production order {production_order_id} expects {} in WIP for {}, but only {wip} \
             is recorded — the quantity state is inconsistent",
            order.qty, order.sku
        )));
    }
    write_stock(
        conn,
        order.item_id,
        on_hand + order.qty,
        on_order,
        wip - order.qty,
    )?;
    conn.execute(
        "UPDATE production_orders SET status = 'completed' WHERE id = ?1",
        params![production_order_id],
    )?;
    list_production_orders(conn)?
        .into_iter()
        .find(|o| o.id == production_order_id)
        .ok_or_else(|| LedgerError::Invalid("production order vanished".to_string()))
}

// ---------------------------------------------------------------------------
// Dates (civil-calendar math, Hinnant's algorithms — no date crate)
// ---------------------------------------------------------------------------

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn parse_date(date: &str) -> Result<i64> {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return Err(LedgerError::Invalid(format!(
            "'{date}' is not a YYYY-MM-DD date"
        )));
    }
    let (y, m, d) = (
        parts[0].parse::<i64>(),
        parts[1].parse::<i64>(),
        parts[2].parse::<i64>(),
    );
    match (y, m, d) {
        (Ok(y), Ok(m), Ok(d)) if (1..=12).contains(&m) && (1..=31).contains(&d) => {
            Ok(days_from_civil(y, m, d))
        }
        _ => Err(LedgerError::Invalid(format!(
            "'{date}' is not a YYYY-MM-DD date"
        ))),
    }
}

fn format_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn add_days(date: &str, delta: i64) -> Result<String> {
    Ok(format_date(parse_date(date)? + delta))
}

fn days_between(later: &str, earlier: &str) -> Result<i64> {
    Ok(parse_date(later)? - parse_date(earlier)?)
}

// ---------------------------------------------------------------------------
// The planning run
// ---------------------------------------------------------------------------

/// Extra (what-if) demand beyond the open sales orders.
#[derive(Debug, Clone)]
pub struct MrpDemand {
    pub sku: String,
    pub qty: i64,
    pub need_date: String,
    pub configuration: Option<BTreeMap<String, ConfigValue>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DemandReason {
    pub origin: String,
    pub qty: i64,
    pub covered_by_stock: i64,
    pub covered_by_plan: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemNetting {
    pub item_id: i64,
    pub sku: String,
    pub gross_required: i64,
    pub on_hand: i64,
    pub on_order: i64,
    pub wip: i64,
    pub available: i64,
    pub net_required: i64,
    pub reasons: Vec<DemandReason>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannedComponent {
    pub sku: String,
    pub qty: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannedOrder {
    /// Stable identity within a plan: kind:sku:due_date:qty.
    /// Release matches on it — recompute the same run, name the key.
    pub key: String,
    /// "production" (make) or "purchase" (buy).
    pub kind: String,
    pub item_id: i64,
    pub sku: String,
    pub qty: i64,
    pub due_date: String,
    pub start_date: String,
    /// > 0 when the start date is already past: this order is a
    /// > shortage the planner should have anticipated.
    pub days_late: i64,
    pub gross_required: i64,
    pub on_hand: i64,
    pub on_order: i64,
    pub wip: i64,
    pub net_required: i64,
    pub reasons: Vec<DemandReason>,
    /// For production orders: what the build consumes.
    pub components: Vec<PlannedComponent>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MrpPlan {
    pub as_of: String,
    pub requirements: Vec<ItemNetting>,
    pub planned_orders: Vec<PlannedOrder>,
    /// The planned orders that start in the past.
    pub shortages: Vec<PlannedOrder>,
}

#[derive(Debug, Clone)]
struct ReqLine {
    qty: i64,
    need_date: String,
    origin: String,
    /// Selected component codes, for configured demand lines.
    config_snapshot: Option<Vec<String>>,
}

/// What one unit of `item_id` consumes: (component item, qty per).
/// Configured lines explode through their snapshot's selected
/// components that map to stock items; BOM lines whose component is
/// configurator-managed are skipped (never double-counted); the
/// remaining BOM lines always apply.
fn explosion_for(
    conn: &Connection,
    item_id: i64,
    config_snapshot: Option<&[String]>,
) -> Result<Vec<(i64, i64)>> {
    // Configurator-managed stock items for this item's model, and
    // the selected subset when a snapshot is present.
    let mut managed: BTreeSet<i64> = BTreeSet::new();
    let mut selected: Vec<(i64, i64)> = Vec::new();
    let model: Option<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM config_models WHERE item_id = ?1")?;
        let mut rows = stmt.query(params![item_id])?;
        match rows.next()? {
            Some(row) => Some(row.get(0)?),
            None => None,
        }
    };
    // A configured line explodes through its snapshot's selected
    // components; an unconfigured line is the standard build and
    // explodes through the BOM alone (below). Either way, BOM lines
    // for configurator-managed components defer to the
    // configuration when a snapshot exists — never double-counted.
    let effective: Option<&[String]> = config_snapshot;
    if let Some(model_id) = model {
        let mut stmt = conn.prepare(
            "SELECT code, component_item_id, qty FROM components
             WHERE model_id = ?1 AND component_item_id IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![model_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (code, comp_item, qty) = row?;
            managed.insert(comp_item);
            if let Some(codes) = effective {
                if codes.iter().any(|c| c == &code) {
                    selected.push((comp_item, qty));
                }
            }
        }
    }
    let mut totals: BTreeMap<i64, i64> = BTreeMap::new();
    for (comp_item, qty) in selected {
        *totals.entry(comp_item).or_default() += qty;
    }
    for line in list_bom(conn, item_id)? {
        if effective.is_some() && managed.contains(&line.component_item_id) {
            continue;
        }
        *totals.entry(line.component_item_id).or_default() += line.qty_per;
    }
    Ok(totals.into_iter().collect())
}

/// Parent → children edges over every item, for level ordering.
fn all_parent_edges(conn: &Connection) -> Result<BTreeMap<i64, BTreeSet<i64>>> {
    let mut edges: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    let item_ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM items ORDER BY id")?;
        let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
        let mut v = Vec::new();
        for r in rows {
            v.push(r?);
        }
        v
    };
    for item_id in item_ids {
        for (child, _) in explosion_for(conn, item_id, None)? {
            edges.entry(item_id).or_default().insert(child);
        }
        // Config links count as edges even without a snapshot, so a
        // configured parent is always planned before its components.
        let mut stmt = conn.prepare(
            "SELECT DISTINCT c.component_item_id FROM components c
             JOIN config_models m ON m.id = c.model_id
             WHERE m.item_id = ?1 AND c.component_item_id IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![item_id], |row| row.get::<_, i64>(0))?;
        for r in rows {
            edges.entry(item_id).or_default().insert(r?);
        }
    }
    Ok(edges)
}

fn item_planning(conn: &Connection, item_id: i64) -> Result<(String, String, i64)> {
    let mut stmt =
        conn.prepare("SELECT sku, make_or_buy, lead_time_days FROM items WHERE id = ?1")?;
    let mut rows = stmt.query(params![item_id])?;
    match rows.next()? {
        Some(row) => Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        None => Err(LedgerError::Invalid(format!("no item with id {item_id}"))),
    }
}

/// Compute a plan from current state. See the module docs.
/// Low-level code of an item over the full BOM graph: 0 when nothing
/// is its parent, else one more than the highest parent's code. Every
/// parent of an item has a strictly smaller code than the item, which
/// is the property the planner's ordering relies on. A cycle in the
/// graph is a data error and refuses the run.
fn low_level_code(
    item: i64,
    parents_of: &BTreeMap<i64, BTreeSet<i64>>,
    memo: &mut BTreeMap<i64, usize>,
    visiting: &mut Vec<i64>,
) -> Result<usize> {
    if let Some(level) = memo.get(&item) {
        return Ok(*level);
    }
    if visiting.contains(&item) {
        return Err(LedgerError::Invalid(
            "the BOM/config structure has a cycle — planning cannot order the items"
                .to_string(),
        ));
    }
    visiting.push(item);
    let mut level = 0usize;
    if let Some(parents) = parents_of.get(&item) {
        for parent in parents {
            level = level.max(low_level_code(*parent, parents_of, memo, visiting)? + 1);
        }
    }
    visiting.pop();
    memo.insert(item, level);
    Ok(level)
}

pub fn run_mrp(conn: &Connection, as_of: &str, extra_demand: &[MrpDemand]) -> Result<MrpPlan> {
    parse_date(as_of)?;
    let mut pool: BTreeMap<i64, Vec<ReqLine>> = BTreeMap::new();

    // Demand from open sales orders. An order is due on the date it
    // was placed (orders carry no promised date — see the design
    // doc); the order view does not expose created_at, so read the
    // dates directly.
    let mut order_dates: BTreeMap<i64, String> = BTreeMap::new();
    {
        let mut stmt = conn.prepare("SELECT id, created_at FROM sales_orders")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, created_at) = row?;
            order_dates.insert(id, created_at.get(..10).unwrap_or(&created_at).to_string());
        }
    }
    for order in list_orders(conn)? {
        if order.status != "open" {
            continue;
        }
        let due = order_dates
            .get(&order.id)
            .cloned()
            .unwrap_or_else(|| as_of.to_string());
        for line in &order.lines {
            let config_snapshot = line.configuration.as_ref().and_then(|cfg| {
                cfg.get("selected_components").and_then(|v| {
                    v.as_array().map(|arr| {
                        arr.iter()
                            .filter_map(|c| c.as_str().map(|s| s.to_string()))
                            .collect::<Vec<String>>()
                    })
                })
            });
            pool.entry(line.item_id).or_default().push(ReqLine {
                qty: line.qty,
                need_date: due.clone(),
                origin: format!("sales order #{} ({} × {})", order.id, line.qty, line.sku),
                config_snapshot,
            });
        }
    }

    // What-if demand.
    for demand in extra_demand {
        if demand.qty <= 0 {
            return Err(LedgerError::Invalid(
                "what-if demand quantity must be positive".to_string(),
            ));
        }
        parse_date(&demand.need_date)?;
        let item_id = item_id_by_sku(conn, &demand.sku)?
            .ok_or_else(|| LedgerError::Invalid(format!("no item with SKU {}", demand.sku)))?;
        let config_snapshot = match &demand.configuration {
            Some(selections) => {
                let model_id: Option<i64> = {
                    let mut stmt =
                        conn.prepare("SELECT id FROM config_models WHERE item_id = ?1")?;
                    let mut rows = stmt.query(params![item_id])?;
                    match rows.next()? {
                        Some(row) => Some(row.get(0)?),
                        None => None,
                    }
                };
                let model_id = model_id.ok_or_else(|| {
                    LedgerError::Invalid(format!(
                        "{} has no configurator model — a configuration cannot be evaluated",
                        demand.sku
                    ))
                })?;
                // The model's `quantity` characteristic is the
                // demand quantity itself (the quote flow merges it
                // the same way), so supply it before evaluating.
                let mut values = selections.clone();
                values.insert("quantity".to_string(), ConfigValue::Int(demand.qty));
                let eval = evaluate_configuration(conn, model_id, &values)?;
                if !matches!(eval.verdict, Verdict::Consistent) {
                    let why = eval
                        .explanations
                        .first()
                        .map(|e| e.message_parts.detail.clone())
                        .unwrap_or_else(|| "the configuration is not consistent".to_string());
                    return Err(LedgerError::Invalid(format!(
                        "what-if configuration for {} refused: {why}",
                        demand.sku
                    )));
                }
                Some(eval.selected_components.clone())
            }
            None => None,
        };
        pool.entry(item_id).or_default().push(ReqLine {
            qty: demand.qty,
            need_date: demand.need_date.clone(),
            origin: format!(
                "what-if demand: {} × {} by {}",
                demand.qty, demand.sku, demand.need_date
            ),
            config_snapshot,
        });
    }

    // Released production orders carry their own component demand
    // (audit fix, Oct 31): a released order's output is counted as
    // WIP supply, so without this step its components would never be
    // demanded — release the parent, re-run, and the steel and
    // fasteners it still needs silently vanish from the plan. The
    // released order's components are firm demand, needed by its
    // start date, netted against component stock like any other line.
    for prod in list_production_orders(conn)? {
        if prod.status != "open" {
            continue;
        }
        for (comp_item, qty_per) in explosion_for(conn, prod.item_id, None)? {
            pool.entry(comp_item).or_default().push(ReqLine {
                qty: prod.qty * qty_per,
                need_date: prod.start_date.clone(),
                origin: format!(
                    "released production order #{} ({} × {})",
                    prod.id, prod.qty, prod.sku
                ),
                config_snapshot: None,
            });
        }
    }

    // Level order (low-level codes over the full BOM graph): a parent
    // is always planned before the components its plan creates demand
    // for — see the selection loop below.
    let edges = all_parent_edges(conn)?;
    let mut parents_of: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    for (parent, children) in &edges {
        for child in children {
            parents_of.entry(*child).or_default().insert(*parent);
        }
    }
    let mut processed: BTreeSet<i64> = BTreeSet::new();
    let mut level_memo: BTreeMap<i64, usize> = BTreeMap::new();

    let mut requirements: Vec<ItemNetting> = Vec::new();
    let mut planned_orders: Vec<PlannedOrder> = Vec::new();

    loop {
        // Plan the unprocessed demanded item with the smallest
        // low-level code. Every parent has a strictly smaller code,
        // so when an item is planned, all demand its parents will
        // ever create is already in its pool lines — including demand
        // from a parent that only enters the pool later, via a
        // grandparent's explosion. (The previous worklist treated a
        // not-yet-demanded parent as no parent at all and silently
        // dropped that late demand — audit fix, Oct 31.)
        let mut next: Option<(usize, i64)> = None;
        for id in pool.keys() {
            if processed.contains(id) {
                continue;
            }
            let level = low_level_code(*id, &parents_of, &mut level_memo, &mut Vec::new())?;
            if next.is_none_or(|(bl, bid)| (level, *id) < (bl, bid)) {
                next = Some((level, *id));
            }
        }
        let item_id = match next {
            Some((_, id)) => id,
            None => break,
        };
        processed.insert(item_id);
        let mut lines = pool.get(&item_id).cloned().unwrap_or_default();
        lines.sort_by(|a, b| (&a.need_date, &a.origin).cmp(&(&b.need_date, &b.origin)));
        let (sku, make_or_buy, lead_days) = item_planning(conn, item_id)?;
        let (on_hand, on_order, wip) = stock_for(conn, item_id)?;
        let available = on_hand + on_order + wip;
        let gross: i64 = lines.iter().map(|l| l.qty).sum();
        let mut remaining = available;
        let mut reasons: Vec<DemandReason> = Vec::new();
        for line in &lines {
            let covered = line.qty.min(remaining.max(0));
            remaining -= covered;
            reasons.push(DemandReason {
                origin: line.origin.clone(),
                qty: line.qty,
                covered_by_stock: covered,
                covered_by_plan: line.qty - covered,
            });
        }
        let net = gross - available;
        requirements.push(ItemNetting {
            item_id,
            sku: sku.clone(),
            gross_required: gross,
            on_hand,
            on_order,
            wip,
            available,
            net_required: net.max(0),
            reasons: reasons.clone(),
        });
        if net <= 0 {
            continue;
        }
        let due = lines
            .iter()
            .zip(reasons.iter())
            .filter(|(_, r)| r.covered_by_plan > 0)
            .map(|(l, _)| l.need_date.clone())
            .min()
            .unwrap_or_else(|| as_of.to_string());
        let start = add_days(&due, -lead_days)?;
        let days_late = days_between(as_of, &start)?.max(0);
        let kind = if make_or_buy == "make" {
            "production"
        } else {
            "purchase"
        };
        // Explode the planned remainder into component demand.
        let mut component_totals: BTreeMap<i64, i64> = BTreeMap::new();
        if kind == "production" {
            for (line, reason) in lines.iter().zip(reasons.iter()) {
                if reason.covered_by_plan <= 0 {
                    continue;
                }
                for (comp_item, qty_per) in
                    explosion_for(conn, item_id, line.config_snapshot.as_deref())?
                {
                    let need = reason.covered_by_plan * qty_per;
                    *component_totals.entry(comp_item).or_default() += need;
                    let origin = format!("planned production of {sku} ×{net} ({})", line.origin);
                    pool.entry(comp_item).or_default().push(ReqLine {
                        qty: need,
                        need_date: start.clone(),
                        origin,
                        config_snapshot: None,
                    });
                }
            }
        }
        let mut components: Vec<PlannedComponent> = Vec::new();
        for (comp_item, qty) in &component_totals {
            let (comp_sku, _, _) = item_planning(conn, *comp_item)?;
            components.push(PlannedComponent {
                sku: comp_sku,
                qty: *qty,
            });
        }
        planned_orders.push(PlannedOrder {
            key: format!("{kind}:{sku}:{due}:{net}"),
            kind: kind.to_string(),
            item_id,
            sku,
            qty: net,
            due_date: due,
            start_date: start,
            days_late,
            gross_required: gross,
            on_hand,
            on_order,
            wip,
            net_required: net,
            reasons: reasons
                .into_iter()
                .filter(|r| r.covered_by_plan > 0)
                .collect(),
            components,
        });
    }

    let shortages: Vec<PlannedOrder> = planned_orders
        .iter()
        .filter(|o| o.days_late > 0)
        .cloned()
        .collect();
    Ok(MrpPlan {
        as_of: as_of.to_string(),
        requirements,
        planned_orders,
        shortages,
    })
}

// ---------------------------------------------------------------------------
// Release
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ReleaseOutcome {
    pub kind: String,
    pub document_id: i64,
    pub sku: String,
    pub qty: i64,
    pub due_date: String,
    pub start_date: Option<String>,
    pub status: String,
    pub note: String,
}

/// Release one planned order from a freshly computed plan: create
/// the production/purchase document and update the quantity state.
/// No journal entry posts — see the module docs.
pub fn release_planned_order(
    conn: &Connection,
    as_of: &str,
    extra_demand: &[MrpDemand],
    key: &str,
) -> Result<ReleaseOutcome> {
    let plan = run_mrp(conn, as_of, extra_demand)?;
    let order = plan
        .planned_orders
        .iter()
        .find(|o| o.key == key)
        .ok_or_else(|| {
            LedgerError::Invalid(format!(
                "no planned order with key '{key}' in the current plan — plans are dynamic and \
             never stored; re-run run_mrp with the same inputs and release a key it returns"
            ))
        })?;
    if order.kind == "production" {
        if let Some(existing) = list_production_orders(conn)?.into_iter().find(|o| {
            o.status == "open"
                && o.item_id == order.item_id
                && o.qty == order.qty
                && o.due_date == order.due_date
        }) {
            return Err(LedgerError::Invalid(format!(
                "planned order {key} was already released as production order #{} — \
                 complete it to bring the units into stock",
                existing.id
            )));
        }
        conn.execute(
            "INSERT INTO production_orders (item_id, qty, due_date, start_date)
             VALUES (?1, ?2, ?3, ?4)",
            params![order.item_id, order.qty, order.due_date, order.start_date],
        )?;
        let id = conn.last_insert_rowid();
        let (on_hand, on_order, wip) = stock_for(conn, order.item_id)?;
        write_stock(conn, order.item_id, on_hand, on_order, wip + order.qty)?;
        Ok(ReleaseOutcome {
            kind: "production".to_string(),
            document_id: id,
            sku: order.sku.clone(),
            qty: order.qty,
            due_date: order.due_date.clone(),
            start_date: Some(order.start_date.clone()),
            status: "open".to_string(),
            note: format!(
                "Released as production order #{id}: {} × {} is now counted as work in \
                 progress, so the next MRP run sees it as supply. No journal entry posts on \
                 release — value moves when materials are issued and production completes \
                 (issue_materials / record_labor / complete_production).",
                order.qty, order.sku
            ),
        })
    } else {
        if let Some(existing) = list_purchase_orders(conn)?.into_iter().find(|o| {
            o.status == "open"
                && o.item_id == order.item_id
                && o.qty == order.qty
                && o.due_date == order.due_date
        }) {
            return Err(LedgerError::Invalid(format!(
                "planned order {key} was already released as purchase order #{} — \
                 receive it to bring the units into stock",
                existing.id
            )));
        }
        conn.execute(
            "INSERT INTO purchase_orders (item_id, qty, due_date) VALUES (?1, ?2, ?3)",
            params![order.item_id, order.qty, order.due_date],
        )?;
        let id = conn.last_insert_rowid();
        let (on_hand, on_order, wip) = stock_for(conn, order.item_id)?;
        write_stock(conn, order.item_id, on_hand, on_order + order.qty, wip)?;
        Ok(ReleaseOutcome {
            kind: "purchase".to_string(),
            document_id: id,
            sku: order.sku.clone(),
            qty: order.qty,
            due_date: order.due_date.clone(),
            start_date: Some(order.start_date.clone()),
            status: "open".to_string(),
            note: format!(
                "Released as purchase order #{id}: {} × {} is now counted as on-order \
                 supply, so the next MRP run sees it. No journal entry posts on release — \
                 value moves when the supplier is paid (pay_supplier).",
                order.qty, order.sku
            ),
        })
    }
}

// ---------------------------------------------------------------------------
// Demo seed (door-level; see MRP_DESIGN.md for why it is not in open_db)
// ---------------------------------------------------------------------------

/// Seed the MRP demo scenario (BOM, config→stock links, lead times,
/// stock levels, one open purchase order) when no BOM exists yet.
/// Returns true when it seeded. Idempotent by the gate: an existing
/// BOM means the scenario (or the operator's own data) is in place.
pub fn seed_mrp_demo_if_empty(conn: &Connection) -> Result<bool> {
    let bom_count: i64 = conn.query_row("SELECT COUNT(*) FROM bom_lines", [], |row| row.get(0))?;
    if bom_count > 0 {
        return Ok(false);
    }
    let insert_item = |sku: &str, name: &str, price: i64| -> Result<i64> {
        conn.execute(
            "INSERT INTO items (sku, name, unit_price_cents, standard_cost_cents)
             VALUES (?1, ?2, ?3, ?3)",
            params![sku, name, price],
        )?;
        Ok(conn.last_insert_rowid())
    };
    let brk = item_id_by_sku(conn, "BRK-100")?
        .ok_or_else(|| LedgerError::Invalid("BRK-100 missing from seed".to_string()))?;
    let steel = insert_item("STL-BLANK", "Steel blank", 1_800)?;
    let aluminum = insert_item("ALU-BLANK", "Aluminum blank", 2_600)?;
    let mount = insert_item("MNT-KIT", "Reinforced mount kit", 950)?;
    let fasteners = insert_item("FST-SET", "Fastener set", 325)?;

    set_item_planning(conn, brk, Some(5), Some("make"))?;
    set_item_planning(conn, steel, Some(7), Some("buy"))?;
    set_item_planning(conn, aluminum, Some(10), Some("buy"))?;
    set_item_planning(conn, mount, Some(4), Some("buy"))?;
    set_item_planning(conn, fasteners, Some(3), Some("buy"))?;

    add_bom_line(conn, brk, steel, 1)?;
    add_bom_line(conn, brk, fasteners, 1)?;

    // Link the configurator's components to the stock items they
    // consume, so configured lines explode into MRP demand.
    for (code, item_id) in [
        ("steel-blank", steel),
        ("aluminum-blank", aluminum),
        ("reinforced-mount", mount),
    ] {
        conn.execute(
            "UPDATE components SET component_item_id = ?2
             WHERE code = ?1 AND component_item_id IS NULL",
            params![code, item_id],
        )?;
    }

    write_stock(conn, brk, 40, 0, 10)?;
    write_stock(conn, steel, 120, 50, 0)?;
    write_stock(conn, aluminum, 25, 0, 0)?;
    write_stock(conn, mount, 30, 0, 0)?;
    write_stock(conn, fasteners, 90, 0, 0)?;

    // The open purchase order behind the steel on-order quantity.
    conn.execute(
        "INSERT INTO purchase_orders (item_id, qty, due_date) VALUES (?1, 50, '2026-11-06')",
        params![steel],
    )?;
    Ok(true)
}
