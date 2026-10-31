//! MRP tests (MRP_DESIGN.md): netting math on hand-computable
//! scenarios, explosion through a configured line, the dynamic
//! property (a change is reflected in the very next run), shortage
//! explanations, and the release flow (documents, not postings).

use erpcraft_core::rusqlite::Connection;
use erpcraft_core::{
    accept_quote, cancel_order, complete_production_order, create_order_from_quote, create_quote,
    list_customers, list_items, list_stock, open_db, receive_purchase_order, release_planned_order,
    run_mrp, seed_mrp_demo_if_empty, send_quote, set_stock, trial_balance, ConfigValue, MrpDemand,
    NewQuote, NewQuoteLine, BRACKET_SKU,
};
use std::collections::BTreeMap;

fn seeded() -> Connection {
    let conn = open_db(":memory:").unwrap();
    assert!(seed_mrp_demo_if_empty(&conn).unwrap());
    conn
}

fn item_id(conn: &Connection, sku: &str) -> i64 {
    list_items(conn)
        .unwrap()
        .into_iter()
        .find(|i| i.sku == sku)
        .unwrap()
        .id
}

fn open_order(conn: &mut Connection, qty: i64) -> i64 {
    let acme = list_customers(conn)
        .unwrap()
        .into_iter()
        .find(|c| c.name == "Acme")
        .unwrap()
        .id;
    let quote_id = create_quote(
        conn,
        &NewQuote {
            customer_id: acme,
            lines: vec![NewQuoteLine {
                item_id: item_id(conn, BRACKET_SKU),
                qty,
                configuration: None,
            }],
        },
    )
    .unwrap();
    send_quote(conn, quote_id).unwrap();
    accept_quote(conn, quote_id).unwrap();
    create_order_from_quote(conn, quote_id).unwrap()
}

fn what_if(qty: i64, need_date: &str) -> Vec<MrpDemand> {
    vec![MrpDemand {
        sku: BRACKET_SKU.to_string(),
        qty,
        need_date: need_date.to_string(),
        configuration: None,
    }]
}

#[test]
fn seed_gives_brk100_a_bom_lead_times_and_stock() {
    let conn = seeded();
    assert!(!seed_mrp_demo_if_empty(&conn).unwrap(), "seed is gated");
    let stock = list_stock(&conn).unwrap();
    assert_eq!(stock.len(), 5);
    let brk = stock.iter().find(|s| s.sku == BRACKET_SKU).unwrap();
    assert_eq!(
        (
            brk.make_or_buy.as_str(),
            brk.lead_time_days,
            brk.on_hand,
            brk.wip
        ),
        ("make", 5, 40, 10)
    );
    let steel = stock.iter().find(|s| s.sku == "STL-BLANK").unwrap();
    assert_eq!(
        (steel.on_hand, steel.on_order, steel.lead_time_days),
        (120, 50, 7)
    );
}

#[test]
fn netting_math_on_a_hand_computable_scenario() {
    let mut conn = seeded();
    open_order(&mut conn, 100);
    let plan = run_mrp(&conn, "2026-10-31", &[]).unwrap();

    // BRK-100: gross 100 against 40 on hand + 10 WIP = 50 available.
    let brk = plan
        .requirements
        .iter()
        .find(|r| r.sku == BRACKET_SKU)
        .unwrap();
    assert_eq!(
        (
            brk.gross_required,
            brk.on_hand,
            brk.on_order,
            brk.wip,
            brk.net_required
        ),
        (100, 40, 0, 10, 50)
    );
    assert_eq!(brk.reasons.len(), 1);
    assert_eq!(
        (
            brk.reasons[0].covered_by_stock,
            brk.reasons[0].covered_by_plan
        ),
        (50, 50)
    );

    // Exactly one planned order: build the uncovered 50.
    assert_eq!(plan.planned_orders.len(), 1);
    let order = &plan.planned_orders[0];
    assert_eq!(
        (order.kind.as_str(), order.sku.as_str(), order.qty),
        ("production", BRACKET_SKU, 50)
    );
    // Components exploded from the planned remainder only.
    let comps: Vec<(&str, i64)> = order
        .components
        .iter()
        .map(|c| (c.sku.as_str(), c.qty))
        .collect();
    assert_eq!(comps, [("STL-BLANK", 50), ("FST-SET", 50)]);

    // Steel: 50 required, 170 available — no order. Fasteners same.
    let steel = plan
        .requirements
        .iter()
        .find(|r| r.sku == "STL-BLANK")
        .unwrap();
    assert_eq!((steel.gross_required, steel.net_required), (50, 0));
}

#[test]
fn what_if_200_by_nov_15_plans_150_and_a_fastener_purchase() {
    let conn = seeded();
    let plan = run_mrp(&conn, "2026-10-31", &what_if(200, "2026-11-15")).unwrap();
    assert!(plan.shortages.is_empty());
    assert_eq!(plan.planned_orders.len(), 2);
    let brk = &plan.planned_orders[0];
    assert_eq!(
        (
            brk.kind.as_str(),
            brk.qty,
            brk.due_date.as_str(),
            brk.start_date.as_str()
        ),
        ("production", 150, "2026-11-15", "2026-11-10")
    );
    assert_eq!(brk.key, "production:BRK-100:2026-11-15:150");
    let fst = &plan.planned_orders[1];
    assert_eq!(
        (
            fst.kind.as_str(),
            fst.sku.as_str(),
            fst.qty,
            fst.due_date.as_str(),
            fst.start_date.as_str()
        ),
        ("purchase", "FST-SET", 60, "2026-11-10", "2026-11-07")
    );
    // The fastener shortage traces back to the BRK-100 build.
    assert!(fst.reasons[0]
        .origin
        .contains("planned production of BRK-100 ×150"));
}

#[test]
fn configured_line_explodes_through_the_configurator_without_double_counting() {
    let conn = seeded();
    let mut config = BTreeMap::new();
    config.insert(
        "material".to_string(),
        ConfigValue::Code("steel".to_string()),
    );
    config.insert(
        "finish".to_string(),
        ConfigValue::Code("galvanized".to_string()),
    );
    config.insert(
        "mounting".to_string(),
        ConfigValue::Code("reinforced".to_string()),
    );
    let demand = vec![MrpDemand {
        sku: BRACKET_SKU.to_string(),
        qty: 100,
        need_date: "2026-11-20".to_string(),
        configuration: Some(config),
    }];
    let plan = run_mrp(&conn, "2026-10-31", &demand).unwrap();

    // BRK-100 plans 50 (100 − 50 available). Its components: the
    // configured steel blank and mount kit, plus BOM fasteners.
    // Steel comes from the configuration ONLY — the BOM's steel line
    // is configurator-managed and must not double-count (gross 50,
    // not 100).
    let steel = plan
        .requirements
        .iter()
        .find(|r| r.sku == "STL-BLANK")
        .unwrap();
    assert_eq!(steel.gross_required, 50);
    // Mount kits: 50 needed, 30 on hand → purchase 20, due at the
    // parent build's start (Nov 20 − 5d = Nov 15), starting Nov 11.
    let mnt = plan
        .planned_orders
        .iter()
        .find(|o| o.sku == "MNT-KIT")
        .unwrap();
    assert_eq!(
        (
            mnt.kind.as_str(),
            mnt.qty,
            mnt.due_date.as_str(),
            mnt.start_date.as_str()
        ),
        ("purchase", 20, "2026-11-15", "2026-11-11")
    );
}

#[test]
fn a_change_is_reflected_in_the_very_next_run() {
    let mut conn = seeded();
    let demand = what_if(200, "2026-11-15");
    let brk_qty = |conn: &Connection| {
        run_mrp(conn, "2026-10-31", &demand)
            .unwrap()
            .planned_orders
            .iter()
            .find(|o| o.sku == BRACKET_SKU)
            .map(|o| o.qty)
    };
    assert_eq!(brk_qty(&conn), Some(150));

    // A new open order lands → the plan grows by exactly its size.
    let order_id = open_order(&mut conn, 30);
    assert_eq!(brk_qty(&conn), Some(180));

    // Cancel it → the plan shrinks back. Nothing was stored.
    cancel_order(&mut conn, order_id).unwrap();
    assert_eq!(brk_qty(&conn), Some(150));

    // A stock correction propagates too: fasteners arrive early.
    let fst_id = item_id(&conn, "FST-SET");
    set_stock(&conn, fst_id, 200, 0, 0).unwrap();
    let plan = run_mrp(&conn, "2026-10-31", &demand).unwrap();
    assert!(
        plan.planned_orders.iter().all(|o| o.sku != "FST-SET"),
        "with 200 fasteners on hand the purchase plan disappears"
    );
}

#[test]
fn a_shortage_explains_itself() {
    let mut conn = seeded();
    open_order(&mut conn, 100);
    // Learn the order's due date from a first run, then re-run as of
    // that date: the build should have started 5 days earlier.
    let first = run_mrp(&conn, "2026-01-01", &[]).unwrap();
    let due = first.planned_orders[0].due_date.clone();
    let plan = run_mrp(&conn, &due, &[]).unwrap();
    assert_eq!(plan.shortages.len(), 1);
    let shortage = &plan.shortages[0];
    assert_eq!(shortage.days_late, 5);
    assert_eq!(shortage.start_date, {
        // due minus the 5-day lead time

        first.planned_orders[0].start_date.clone()
    });
    let reason = &shortage.reasons[0];
    assert!(reason.origin.contains("sales order #"));
    assert_eq!(
        (reason.qty, reason.covered_by_stock, reason.covered_by_plan),
        (100, 50, 50)
    );
}

#[test]
fn release_creates_documents_not_postings() {
    let conn = seeded();
    let demand = what_if(200, "2026-11-15");

    // Release the fastener purchase first.
    let po = release_planned_order(
        &conn,
        "2026-10-31",
        &demand,
        "purchase:FST-SET:2026-11-10:60",
    )
    .unwrap();
    assert_eq!((po.kind.as_str(), po.document_id), ("purchase", 2));
    let fst = list_stock(&conn)
        .unwrap()
        .into_iter()
        .find(|s| s.sku == "FST-SET")
        .unwrap();
    assert_eq!(fst.on_order, 60);

    // Release the BRK-100 build.
    let wo = release_planned_order(
        &conn,
        "2026-10-31",
        &demand,
        "production:BRK-100:2026-11-15:150",
    )
    .unwrap();
    assert_eq!((wo.kind.as_str(), wo.document_id), ("production", 1));

    // No journal entries posted: the trial balance is still exactly
    // the opening $120,500.00 per side.
    let tb = trial_balance(&conn).unwrap();
    let total_debits: i64 = tb.rows.iter().map(|r| r.debit_cents).sum();
    assert_eq!(total_debits, 12_050_000);

    // The released supply is visible to the next run: same demand,
    // nothing left to plan.
    let plan = run_mrp(&conn, "2026-10-31", &demand).unwrap();
    assert!(plan.planned_orders.is_empty());

    // Releasing a key the current plan no longer returns is refused.
    let err = release_planned_order(
        &conn,
        "2026-10-31",
        &demand,
        "purchase:FST-SET:2026-11-10:60",
    )
    .unwrap_err();
    assert!(err.to_string().contains("no planned order with key"));

    // Completing the build and receiving the purchase move
    // quantities into on-hand.
    complete_production_order(&conn, 1).unwrap();
    receive_purchase_order(&conn, 2).unwrap();
    let stock = list_stock(&conn).unwrap();
    let brk = stock.iter().find(|s| s.sku == BRACKET_SKU).unwrap();
    assert_eq!((brk.on_hand, brk.wip), (190, 10));
    let fst = stock.iter().find(|s| s.sku == "FST-SET").unwrap();
    assert_eq!((fst.on_hand, fst.on_order), (150, 0));
}

// ---------------------------------------------------------------------------
// Audit regressions (Oct 31): the two demand-loss defects the external
// review reproduced — released parents erasing component demand, and
// multi-level converging demand undercounted by worklist order.
// ---------------------------------------------------------------------------

#[test]
fn releasing_a_parent_keeps_its_component_demand() {
    let conn = seeded();
    for s in list_stock(&conn).unwrap() {
        set_stock(&conn, s.item_id, 0, 0, 0).unwrap();
    }
    let demand = vec![MrpDemand {
        sku: BRACKET_SKU.to_string(),
        qty: 10,
        need_date: "2026-11-15".to_string(),
        configuration: None,
    }];
    let plan = run_mrp(&conn, "2026-10-31", &demand).unwrap();
    assert_eq!(plan.planned_orders.len(), 3, "production + steel + fasteners");
    let production_key = plan
        .planned_orders
        .iter()
        .find(|o| o.kind == "production")
        .unwrap()
        .key
        .clone();
    release_planned_order(&conn, "2026-10-31", &demand, &production_key).unwrap();

    // The rerun must NOT re-plan production (WIP covers it) — and must
    // still plan the components the released order needs.
    let after = run_mrp(&conn, "2026-10-31", &demand).unwrap();
    assert!(
        after.planned_orders.iter().all(|o| o.kind != "production"),
        "released production is supply, not new demand: {:?}",
        after.planned_orders.iter().map(|o| (&o.sku, o.qty)).collect::<Vec<_>>()
    );
    let purchased: i64 = after
        .planned_orders
        .iter()
        .filter(|o| o.kind == "purchase")
        .map(|o| o.qty)
        .sum();
    assert_eq!(purchased, 20, "steel 10 + fasteners 10 survive the release");
    let steel_req = after
        .requirements
        .iter()
        .find(|r| r.gross_required > 0 && r.reasons.iter().any(|x| x.origin.contains("released production order")))
        .expect("component demand is explained by the released order");
    assert!(steel_req.gross_required >= 10);
}

#[test]
fn converging_multilevel_demand_is_fully_counted() {
    let conn = seeded();
    // A (make) -> B (make) -> BRK-100, with independent demand on both
    // A and BRK-100. BRK-100's gross must be 10 (independent) + 10
    // (via A and B) = 20, even though B only enters the pool when A
    // explodes.
    conn.execute(
        "INSERT INTO items (sku, name, unit_price_cents, standard_cost_cents, make_or_buy)
         VALUES ('A', 'A', 100, 50, 'make')",
        [],
    )
    .unwrap();
    let a = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO items (sku, name, unit_price_cents, standard_cost_cents, make_or_buy)
         VALUES ('B', 'B', 100, 50, 'make')",
        [],
    )
    .unwrap();
    let b = conn.last_insert_rowid();
    let bracket = item_id(&conn, BRACKET_SKU);
    erpcraft_core::add_bom_line(&conn, a, b, 1).unwrap();
    erpcraft_core::add_bom_line(&conn, b, bracket, 1).unwrap();
    let demand = vec![
        MrpDemand {
            sku: "A".to_string(),
            qty: 10,
            need_date: "2026-11-15".to_string(),
            configuration: None,
        },
        MrpDemand {
            sku: BRACKET_SKU.to_string(),
            qty: 10,
            need_date: "2026-11-15".to_string(),
            configuration: None,
        },
    ];
    let plan = run_mrp(&conn, "2026-10-31", &demand).unwrap();
    let bracket_req = plan
        .requirements
        .iter()
        .find(|r| r.sku == BRACKET_SKU)
        .unwrap();
    assert_eq!(bracket_req.gross_required, 20);
}
