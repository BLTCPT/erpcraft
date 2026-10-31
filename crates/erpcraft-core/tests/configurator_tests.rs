//! Variant configurator tests for the ERPCraft core.
//!
//! The contract (CONFIGURATOR_DESIGN.md): dependencies are typed data,
//! evaluation is pure and deterministic, every rejection carries a
//! structured explanation, quote-line snapshots are immutable, and
//! configured lines price by rolling the selected components' deltas
//! into the item's base price and standard cost — while unconfigured
//! lines price exactly as Day 2 priced them.

use erpcraft_core::rusqlite::Connection;
use erpcraft_core::{
    accept_quote, config_model_for_item, create_component, create_dependency,
    create_order_from_quote, create_quote, dependency_fingerprint, entry_count, evaluate,
    evaluate_configuration, get_order, get_quote, invoice_count, list_customers, list_items,
    open_db, order_count, payment_count, quote_count, send_quote, trial_balance, ConfigModelView,
    ConfigValue, Expr, NewComponent, NewDependency, NewQuote, NewQuoteLine, Verdict, BRACKET_SKU,
    BRACKET_UNIT_PRICE_CENTS, OPENING_TOTAL_CENTS, STANDARD_COST_CENTS,
};
use std::collections::BTreeMap;

fn bracket_id(conn: &Connection) -> i64 {
    list_items(conn)
        .unwrap()
        .into_iter()
        .find(|i| i.sku == BRACKET_SKU)
        .expect("bracket item is seeded")
        .id
}

fn customer_id(conn: &Connection, name: &str) -> i64 {
    list_customers(conn)
        .unwrap()
        .into_iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("customer {name} is seeded"))
        .id
}

fn demo_model(conn: &Connection) -> ConfigModelView {
    config_model_for_item(conn, bracket_id(conn))
        .unwrap()
        .expect("the BRK-100 demo model is seeded")
}

fn dep_id(model: &ConfigModelView, kind: &str, target: &str) -> i64 {
    model
        .dependencies
        .iter()
        .find(|d| d.kind == kind && d.target == target)
        .unwrap_or_else(|| panic!("dependency {kind}/{target} is seeded"))
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

fn configured_quote(
    conn: &mut Connection,
    qty: i64,
    configuration: BTreeMap<String, ConfigValue>,
) -> NewQuote {
    NewQuote {
        customer_id: customer_id(conn, "Acme"),
        lines: vec![NewQuoteLine {
            item_id: bracket_id(conn),
            qty,
            configuration: Some(configuration),
        }],
    }
}

// ---------------------------------------------------------------------------
// 1. Precondition pruning removes a value and rejects it if chosen.
// ---------------------------------------------------------------------------

#[test]
fn precondition_prunes_galvanized_for_aluminum_and_rejects_it_if_chosen() {
    let conn = open_db(":memory:").unwrap();
    let model = demo_model(&conn);
    let precondition_id = dep_id(&model, "precondition", "finish=galvanized");

    // With material = aluminum, galvanized is out of play.
    let eval = evaluate(
        &model,
        &config(&[
            ("material", code("aluminum")),
            ("quantity", ConfigValue::Int(5)),
        ]),
    );
    assert_eq!(
        eval.selectable.get("finish").unwrap(),
        &vec!["bare".to_string(), "powder-coat".to_string()],
        "galvanized must be pruned for aluminum"
    );

    // Choosing it anyway is a conflict, explained by the precondition.
    let eval = evaluate(
        &model,
        &config(&[
            ("material", code("aluminum")),
            ("finish", code("galvanized")),
            ("quantity", ConfigValue::Int(5)),
        ]),
    );
    assert!(matches!(eval.verdict, Verdict::Inconsistent));
    assert!(
        eval.explanations
            .iter()
            .any(|e| e.kind == "precondition" && e.dependency_id == Some(precondition_id)),
        "the precondition must explain the rejection: {:?}",
        eval.explanations
    );
}

// ---------------------------------------------------------------------------
// 1b. A conflict already reported by its own dependency must not ALSO
//     spawn "dead end" explanations for unrelated characteristics:
//     with a broken assignment in place, every remaining value is
//     trivially un-completable, and blaming the same dependency for
//     each one just repeats its sentence (the demo polish fix).
// ---------------------------------------------------------------------------

#[test]
fn a_conflict_does_not_also_report_spurious_dead_ends() {
    let conn = open_db(":memory:").unwrap();
    let model = demo_model(&conn);
    let precondition_id = dep_id(&model, "precondition", "finish=galvanized");

    let eval = evaluate(
        &model,
        &config(&[
            ("material", code("aluminum")),
            ("finish", code("galvanized")),
            ("quantity", ConfigValue::Int(5)),
        ]),
    );
    assert!(matches!(eval.verdict, Verdict::Inconsistent));
    assert_eq!(
        eval.explanations.len(),
        1,
        "exactly one explanation — the precondition conflict itself: {:?}",
        eval.explanations
    );
    assert_eq!(eval.explanations[0].dependency_id, Some(precondition_id));
    // The selectable-space analysis itself is unchanged: no mounting
    // value can complete this (broken) configuration.
    assert!(eval.selectable.get("mounting").unwrap().is_empty());
}

#[test]
fn a_genuine_dead_end_still_explains_itself() {
    let conn = open_db(":memory:").unwrap();
    let model = demo_model(&conn);
    // Both mounting values require a bare finish in this test's model.
    for target in ["mounting=standard", "mounting=reinforced"] {
        create_dependency(
            &conn,
            model.id,
            &NewDependency {
                kind: "precondition".to_string(),
                target: target.to_string(),
                expr: Expr::Eq {
                    char_code: "finish".to_string(),
                    value: serde_json::Value::String("bare".to_string()),
                },
                source_text: format!("{target} is offered only on bare brackets."),
                created_by: "test".to_string(),
            },
        )
        .unwrap();
    }
    let model = demo_model(&conn);

    // Powder-coat is a perfectly consistent choice — but it leaves
    // mounting with no available value at all: a real dead end, and
    // the only problem with this configuration.
    let eval = evaluate(
        &model,
        &config(&[
            ("material", code("steel")),
            ("finish", code("powder-coat")),
            ("quantity", ConfigValue::Int(5)),
        ]),
    );
    assert!(matches!(eval.verdict, Verdict::Inconsistent));
    assert_eq!(
        eval.explanations.len(),
        2,
        "one dead-end explanation per eliminated mounting value: {:?}",
        eval.explanations
    );
    for e in &eval.explanations {
        assert_eq!(
            e.message_parts.characteristics,
            vec!["mounting".to_string()]
        );
    }
    let blamed: Vec<&str> = eval
        .explanations
        .iter()
        .map(|e| e.message_parts.values[0].as_str())
        .collect();
    assert!(blamed.contains(&"mounting=standard"));
    assert!(blamed.contains(&"mounting=reinforced"));
}

// ---------------------------------------------------------------------------
// 2. Selection conditions and constraint narrowing force values and
//    components — each naming the dependency responsible.
// ---------------------------------------------------------------------------

#[test]
fn selections_force_values_and_components_and_name_themselves() {
    let conn = open_db(":memory:").unwrap();
    let model = demo_model(&conn);
    let constraint_id = dep_id(
        &model,
        "constraint",
        "quantity-requires-reinforced-mounting",
    );
    let mount_selection_id = dep_id(&model, "selection", "component:reinforced-mount");

    // Steel + galvanized at qty 50: the constraint narrows mounting to
    // exactly one candidate, so the evaluator forces it — and the
    // reinforced-mount selection then fires on the forced value.
    let eval = evaluate(
        &model,
        &config(&[
            ("material", code("steel")),
            ("finish", code("galvanized")),
            ("quantity", ConfigValue::Int(50)),
        ]),
    );
    assert!(matches!(eval.verdict, Verdict::Consistent));
    let forced_mounting = eval
        .forced
        .get("mounting")
        .expect("mounting is forced at qty 50");
    assert_eq!(forced_mounting.value, code("reinforced"));
    assert_eq!(forced_mounting.dependency_id, constraint_id);
    assert_eq!(forced_mounting.kind, "constraint");
    assert!(eval
        .forced_components
        .iter()
        .any(|f| f.component == "reinforced-mount" && f.dependency_id == mount_selection_id));
    assert_eq!(
        eval.selected_components,
        vec![
            "steel-blank".to_string(),
            "galvanized-finish".to_string(),
            "reinforced-mount".to_string()
        ]
    );

    // A selection condition forcing a characteristic value names itself
    // in `forced` too. (Added to this test's own model instance.)
    let selection_id = create_dependency(
        &conn,
        model.id,
        &NewDependency {
            kind: "selection".to_string(),
            target: "finish=powder-coat".to_string(),
            expr: Expr::Eq {
                char_code: "material".to_string(),
                value: serde_json::Value::String("aluminum".to_string()),
            },
            source_text: "In this test, aluminum brackets always take powder-coat.".to_string(),
            created_by: "test".to_string(),
        },
    )
    .unwrap();
    let model = demo_model(&conn);
    let eval = evaluate(
        &model,
        &config(&[
            ("material", code("aluminum")),
            ("quantity", ConfigValue::Int(5)),
        ]),
    );
    let forced_finish = eval.forced.get("finish").expect("finish is forced");
    assert_eq!(forced_finish.value, code("powder-coat"));
    assert_eq!(forced_finish.dependency_id, selection_id);
    assert_eq!(forced_finish.kind, "selection");
    // The forced finish drives its component selection in turn.
    assert!(eval
        .selected_components
        .iter()
        .any(|c| c == "powder-coat-finish"));
}

// ---------------------------------------------------------------------------
// 3. Constraint violation -> Inconsistent, with the firing dependency
//    and the offending values in the explanation.
// ---------------------------------------------------------------------------

#[test]
fn constraint_violation_reports_the_dependency_and_values() {
    let conn = open_db(":memory:").unwrap();
    let model = demo_model(&conn);
    let constraint_id = dep_id(
        &model,
        "constraint",
        "quantity-requires-reinforced-mounting",
    );

    let eval = evaluate(
        &model,
        &config(&[
            ("material", code("steel")),
            ("finish", code("bare")),
            ("mounting", code("standard")),
            ("quantity", ConfigValue::Int(50)),
        ]),
    );
    assert!(matches!(eval.verdict, Verdict::Inconsistent));
    let explanation = eval
        .explanations
        .iter()
        .find(|e| e.kind == "constraint" && e.dependency_id == Some(constraint_id))
        .expect("the constraint explains itself");
    assert!(
        explanation
            .message_parts
            .values
            .contains(&"quantity=50".to_string()),
        "offending values are named: {:?}",
        explanation.message_parts.values
    );
    assert!(explanation
        .message_parts
        .values
        .contains(&"mounting=standard".to_string()));
    assert_eq!(
        explanation.message_parts.detail,
        "Orders over 40 units need the reinforced mount."
    );
}

// ---------------------------------------------------------------------------
// 4. Determinism: same model + same partial config -> byte-identical
//    Evaluation, across calls and across model loads.
// ---------------------------------------------------------------------------

#[test]
fn evaluation_is_byte_identical_across_calls_and_loads() {
    let conn = open_db(":memory:").unwrap();
    let input = config(&[
        ("material", code("steel")),
        ("finish", code("galvanized")),
        ("quantity", ConfigValue::Int(50)),
    ]);
    let model = demo_model(&conn);
    let first = serde_json::to_string(&evaluate(&model, &input)).unwrap();
    let second = serde_json::to_string(&evaluate(&model, &input)).unwrap();
    assert_eq!(first, second);

    let reloaded = demo_model(&conn);
    let third = serde_json::to_string(&evaluate(&reloaded, &input)).unwrap();
    assert_eq!(first, third);

    let via_db = evaluate_configuration(&conn, model.id, &input).unwrap();
    assert_eq!(first, serde_json::to_string(&via_db).unwrap());
}

// ---------------------------------------------------------------------------
// 5. Snapshot immutability: editing a dependency after quoting does
//    not change the quote's stored configuration or evaluation record.
// ---------------------------------------------------------------------------

#[test]
fn quote_snapshot_survives_dependency_edits() {
    let mut conn = open_db(":memory:").unwrap();
    let model = demo_model(&conn);
    let precondition_id = dep_id(&model, "precondition", "finish=galvanized");

    let new_quote = configured_quote(
        &mut conn,
        10,
        config(&[
            ("material", code("steel")),
            ("finish", code("galvanized")),
            ("mounting", code("standard")),
        ]),
    );
    let quote_id = create_quote(&mut conn, &new_quote).unwrap();
    let quote = get_quote(&conn, quote_id).unwrap().unwrap();
    let line = &quote.lines[0];
    // Steel + galvanized: $125.00 base + $15.00 galvanizing.
    assert_eq!(line.unit_price_cents, 14_000);
    let snapshot_before = line.configuration.clone().expect("snapshot stored");
    let fingerprint_before = snapshot_before["dependency_fingerprint"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        fingerprint_before,
        dependency_fingerprint(&model),
        "the snapshot records the dependency set it was evaluated against"
    );

    // Rewrite the precondition so galvanized now requires *aluminum*.
    conn.execute(
        "UPDATE dependencies SET expr = ?1 WHERE id = ?2",
        rusqlite::params![
            r#"{"type":"eq","char":"material","value":"aluminum"}"#,
            precondition_id
        ],
    )
    .unwrap();

    // The stored snapshot and price are untouched...
    let quote = get_quote(&conn, quote_id).unwrap().unwrap();
    assert_eq!(quote.lines[0].unit_price_cents, 14_000);
    assert_eq!(
        quote.lines[0].configuration.as_ref(),
        Some(&snapshot_before)
    );

    // ...while a fresh evaluation under the edited model really does
    // reject the same configuration (the edit took effect).
    let model = demo_model(&conn);
    assert_ne!(dependency_fingerprint(&model), fingerprint_before);
    let eval = evaluate(
        &model,
        &config(&[
            ("material", code("steel")),
            ("finish", code("galvanized")),
            ("mounting", code("standard")),
            ("quantity", ConfigValue::Int(10)),
        ]),
    );
    assert!(matches!(eval.verdict, Verdict::Inconsistent));
}

// ---------------------------------------------------------------------------
// 6. No-GL guard, configurator edition: evaluating a configuration
//    creates zero journal entries and zero documents — and under Day 3
//    that still holds exactly: evaluation and quote creation post
//    nothing (only issuance and payments post, per the posting map).
// ---------------------------------------------------------------------------

#[test]
fn evaluation_creates_no_journal_entries_and_no_documents() {
    let mut conn = open_db(":memory:").unwrap();
    let entries_before = entry_count(&conn).unwrap();
    let tb_before = trial_balance(&conn).unwrap();
    assert_eq!(tb_before.total_debit_cents, OPENING_TOTAL_CENTS);
    let docs_before = (
        quote_count(&conn).unwrap(),
        order_count(&conn).unwrap(),
        invoice_count(&conn).unwrap(),
        payment_count(&conn).unwrap(),
    );

    let model = demo_model(&conn);
    for input in [
        config(&[("material", code("steel"))]),
        config(&[
            ("material", code("aluminum")),
            ("finish", code("galvanized")),
            ("quantity", ConfigValue::Int(99)),
        ]),
        config(&[
            ("material", code("steel")),
            ("finish", code("galvanized")),
            ("quantity", ConfigValue::Int(50)),
        ]),
    ] {
        let _ = evaluate(&model, &input);
        let _ = evaluate_configuration(&conn, model.id, &input).unwrap();
        let _ = dependency_fingerprint(&model);
    }

    assert_eq!(entry_count(&conn).unwrap(), entries_before);
    assert_eq!(
        (
            quote_count(&conn).unwrap(),
            order_count(&conn).unwrap(),
            invoice_count(&conn).unwrap(),
            payment_count(&conn).unwrap()
        ),
        docs_before
    );
    let tb_after = trial_balance(&conn).unwrap();
    assert_eq!(tb_after.total_debit_cents, tb_before.total_debit_cents);
    assert_eq!(tb_after.total_credit_cents, tb_before.total_credit_cents);

    // Day-3 sharpening: even *using* an evaluation — creating a quote
    // with a configured line — posts nothing. Only issuance and payment
    // transitions post, and this test never reaches them.
    let new_quote = configured_quote(
        &mut conn,
        50,
        config(&[("material", code("steel")), ("finish", code("galvanized"))]),
    );
    create_quote(&mut conn, &new_quote).unwrap();
    assert_eq!(entry_count(&conn).unwrap(), entries_before);
    assert_eq!(invoice_count(&conn).unwrap(), docs_before.2);
    assert_eq!(payment_count(&conn).unwrap(), docs_before.3);
    let tb_final = trial_balance(&conn).unwrap();
    assert_eq!(tb_final.total_debit_cents, tb_before.total_debit_cents);
    assert_eq!(tb_final.total_credit_cents, tb_before.total_credit_cents);
}

// ---------------------------------------------------------------------------
// 7. Pricing roll-up: a configured line's snapshotted price/cost equal
//    base + sum of selected component deltas; unselected components
//    contribute nothing; an unconfigured line prices exactly as Day 2.
// ---------------------------------------------------------------------------

#[test]
fn configured_lines_roll_selected_components_into_price_and_cost() {
    let mut conn = open_db(":memory:").unwrap();

    // Steel + galvanized, qty 50 (mounting forced to reinforced):
    // price $125.00 + $15.00 galvanized + $22.00 reinforced = $162.00
    // cost   $68.00 +  $9.00 galvanized + $14.00 reinforced =  $91.00
    let new_quote = configured_quote(
        &mut conn,
        50,
        config(&[("material", code("steel")), ("finish", code("galvanized"))]),
    );
    let quote_id = create_quote(&mut conn, &new_quote).unwrap();
    let quote = get_quote(&conn, quote_id).unwrap().unwrap();
    let line = &quote.lines[0];
    assert_eq!(line.unit_price_cents, 16_200);
    assert_eq!(line.standard_cost_cents, Some(9_100));
    assert_eq!(line.line_total_cents, 50 * 16_200);
    let snapshot = line.configuration.as_ref().unwrap();
    assert_eq!(
        snapshot["selected_components"],
        serde_json::json!(["steel-blank", "galvanized-finish", "reinforced-mount"])
    );
    assert_eq!(
        snapshot["values"]["mounting"],
        serde_json::json!("reinforced")
    );

    // Aluminum + bare + standard, qty 10, plus one unconfigured line:
    // only the aluminum blank rolls up ($18.00 / $11.00); the steel
    // blank, finishes and mount kit contribute nothing, and the plain
    // line prices exactly as Day 2 priced it.
    let mut new_quote = configured_quote(
        &mut conn,
        10,
        config(&[
            ("material", code("aluminum")),
            ("finish", code("bare")),
            ("mounting", code("standard")),
        ]),
    );
    new_quote.lines.push(NewQuoteLine {
        item_id: bracket_id(&conn),
        qty: 3,
        configuration: None,
    });
    let quote_id = create_quote(&mut conn, &new_quote).unwrap();
    let quote = get_quote(&conn, quote_id).unwrap().unwrap();
    let configured = &quote.lines[0];
    assert_eq!(
        configured.unit_price_cents,
        BRACKET_UNIT_PRICE_CENTS + 1_800
    );
    assert_eq!(
        configured.standard_cost_cents,
        Some(STANDARD_COST_CENTS + 1_100)
    );
    let plain = &quote.lines[1];
    assert_eq!(plain.unit_price_cents, BRACKET_UNIT_PRICE_CENTS);
    assert_eq!(plain.standard_cost_cents, Some(STANDARD_COST_CENTS));
    assert!(plain.configuration.is_none());
    assert_eq!(
        quote.total_cents,
        10 * (BRACKET_UNIT_PRICE_CENTS + 1_800) + 3 * BRACKET_UNIT_PRICE_CENTS
    );
}

// ---------------------------------------------------------------------------
// 8. Acceptance gate: incomplete configurations may be drafted but
//    never accepted; consistent ones flow through to the order with
//    their snapshot intact.
// ---------------------------------------------------------------------------

#[test]
fn acceptance_requires_a_consistent_configuration() {
    let mut conn = open_db(":memory:").unwrap();

    // Only the material chosen: finish and mounting are missing.
    let new_quote = configured_quote(&mut conn, 10, config(&[("material", code("steel"))]));
    let quote_id = create_quote(&mut conn, &new_quote).unwrap();
    send_quote(&mut conn, quote_id).unwrap();
    let err = accept_quote(&mut conn, quote_id).unwrap_err();
    assert!(
        err.to_string().contains("incomplete"),
        "acceptance must refuse an incomplete configuration: {err}"
    );
    assert!(err.to_string().contains("finish"), "got: {err}");

    // A complete, consistent configuration accepts, and the order
    // copies the snapshot and the rolled-up price unchanged.
    let new_quote = configured_quote(
        &mut conn,
        10,
        config(&[
            ("material", code("steel")),
            ("finish", code("galvanized")),
            ("mounting", code("standard")),
        ]),
    );
    let quote_id = create_quote(&mut conn, &new_quote).unwrap();
    send_quote(&mut conn, quote_id).unwrap();
    accept_quote(&mut conn, quote_id).unwrap();
    let order_id = create_order_from_quote(&mut conn, quote_id).unwrap();
    let order = get_order(&conn, order_id).unwrap().unwrap();
    assert_eq!(order.lines[0].unit_price_cents, 14_000);
    assert_eq!(
        order.lines[0].configuration,
        get_quote(&conn, quote_id).unwrap().unwrap().lines[0].configuration
    );
}

// ---------------------------------------------------------------------------
// 9. Dependency save-time validation: unknown characteristics,
//    ill-typed expressions, and cycles are all refused at save time.
// ---------------------------------------------------------------------------

#[test]
fn dependency_save_validation_and_cycle_rejection() {
    let conn = open_db(":memory:").unwrap();
    let model = demo_model(&conn);
    let eq = |char_code: &str, value: &str| Expr::Eq {
        char_code: char_code.to_string(),
        value: serde_json::Value::String(value.to_string()),
    };
    let dep = |kind: &str, target: &str, expr: Expr| NewDependency {
        kind: kind.to_string(),
        target: target.to_string(),
        expr,
        source_text: String::new(),
        created_by: "test".to_string(),
    };

    // Unknown characteristic in the expression.
    assert!(create_dependency(&conn, model.id, &dep("constraint", "x", eq("nope", "a"))).is_err());
    // A comparison against an enum characteristic.
    assert!(create_dependency(
        &conn,
        model.id,
        &dep(
            "constraint",
            "x",
            Expr::Cmp {
                char_code: "material".to_string(),
                op: erpcraft_core::CmpOp::Gt,
                value: 3,
            },
        ),
    )
    .is_err());
    // A value outside the enum domain.
    assert!(create_dependency(
        &conn,
        model.id,
        &dep("constraint", "x", eq("material", "chrome"))
    )
    .is_err());
    // An unknown kind.
    assert!(create_dependency(
        &conn,
        model.id,
        &dep("procedure", "x", eq("material", "steel"))
    )
    .is_err());

    // A cycle: finish is driven by material, then material by finish.
    create_dependency(
        &conn,
        model.id,
        &dep("selection", "finish=bare", eq("material", "steel")),
    )
    .unwrap();
    let err = create_dependency(
        &conn,
        model.id,
        &dep("selection", "material=steel", eq("finish", "bare")),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("cycle"),
        "the closing edge of a cycle is refused: {err}"
    );
}

// ---------------------------------------------------------------------------
// 10. A rolled-up price below zero is a constraint violation, never
//     silently clamped.
// ---------------------------------------------------------------------------

#[test]
fn negative_rolled_up_price_is_refused_not_clamped() {
    let mut conn = open_db(":memory:").unwrap();
    let model = demo_model(&conn);
    create_component(
        &conn,
        model.id,
        &NewComponent {
            code: "delete-credit".to_string(),
            name: "Delete credit".to_string(),
            component_item_id: None,
            qty: 1,
            price_delta_cents: -20_000,
            cost_delta_cents: 0,
        },
    )
    .unwrap();
    create_dependency(
        &conn,
        model.id,
        &NewDependency {
            kind: "selection".to_string(),
            target: "component:delete-credit".to_string(),
            expr: Expr::Eq {
                char_code: "material".to_string(),
                value: serde_json::Value::String("aluminum".to_string()),
            },
            source_text: String::new(),
            created_by: "test".to_string(),
        },
    )
    .unwrap();

    // $125.00 + $18.00 (aluminum blank) - $200.00 (delete credit) < 0.
    let new_quote = configured_quote(
        &mut conn,
        5,
        config(&[
            ("material", code("aluminum")),
            ("finish", code("bare")),
            ("mounting", code("standard")),
        ]),
    );
    let err = create_quote(&mut conn, &new_quote).unwrap_err();
    assert!(
        err.to_string().contains("below zero"),
        "a negative rolled-up price is reported, not clamped: {err}"
    );
}

// ---------------------------------------------------------------------------
// Audit regression (Oct 31): a selection dependency forcing a value
// outside the characteristic's domain ("material=unobtainium") used to
// save cleanly and evaluate as Consistent.
// ---------------------------------------------------------------------------

#[test]
fn selection_forcing_a_non_domain_value_is_refused_at_save() {
    let conn = open_db(":memory:").unwrap();
    let err = erpcraft_core::create_dependency(
        &conn,
        1,
        &erpcraft_core::NewDependency {
            kind: "selection".to_string(),
            target: "material=unobtainium".to_string(),
            expr: erpcraft_core::Expr::And { all: vec![] },
            source_text: "force non-domain material".to_string(),
            created_by: "test".to_string(),
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("not a legal value"), "got: {err}");
}

#[test]
fn legacy_invalid_forced_value_evaluates_inconsistent() {
    // A dep stored before save-time validation existed (inserted
    // directly, bypassing create_dependency) must still not yield a
    // Consistent verdict with an impossible value forced in.
    let conn = open_db(":memory:").unwrap();
    conn.execute(
        "INSERT INTO dependencies (model_id, kind, target, expr, source_text, created_by)
         VALUES (1, 'selection', 'material=unobtainium', '{\"type\":\"and\",\"all\":[]}', 'legacy', 'test')",
        [],
    )
    .unwrap();
    let config = std::collections::BTreeMap::from([
        ("finish".to_string(), ConfigValue::Code("bare".to_string())),
        ("mounting".to_string(), ConfigValue::Code("standard".to_string())),
        ("quantity".to_string(), ConfigValue::Int(1)),
    ]);
    let eval = erpcraft_core::evaluate_configuration(&conn, 1, &config).unwrap();
    assert!(
        !matches!(eval.verdict, erpcraft_core::Verdict::Consistent),
        "an impossible forced value cannot be Consistent: {eval:?}"
    );
}
