//! The binary contract (exit test 4's CLI half + dispatch coverage):
//! JSON on stdout, plain-language refusals on stderr, exit 0 ok /
//! 1 refused-or-invalid / 2 internal. Every refusal leaves the book
//! exactly as it was.

use serde_json::Value;
use std::process::{Command, Output};

fn run(db: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_erpcraft"))
        .args(args)
        .env("ERPCRAFT_DB", db)
        .output()
        .expect("the erpcraft binary runs")
}

fn run_json(db: &std::path::Path, args: &[&str]) -> (Value, Output) {
    let out = run(db, args);
    let stdout = String::from_utf8(out.stdout.clone()).unwrap();
    let parsed: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is one JSON document ({e}): {stdout}"));
    (parsed, out)
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

fn temp_db(tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "erpcraft-cli-contract-{tag}-{}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

/// Drive a plain 40-bracket quote all the way to an issued invoice;
/// returns the invoice id. Used by the refusal tests.
fn issued_invoice(db: &std::path::Path) -> i64 {
    let (quote, out) = run_json(
        db,
        &[
            "create_quote",
            "--json",
            r#"{"customer_id": 1, "lines": [{"item_id": 1, "qty": 40}]}"#,
        ],
    );
    assert!(out.status.success());
    let qid = quote["id"].as_i64().unwrap().to_string();
    for (cmd, arg) in [("send_quote", "quote_id"), ("accept_quote", "quote_id")] {
        let flag = format!("--{}", arg.replace('_', "-"));
        let (_, out) = run_json(db, &[cmd, &flag, &qid]);
        assert!(out.status.success(), "{cmd}: {}", stderr(&out));
    }
    let (order, out) = run_json(db, &["create_order", &qid]);
    assert!(out.status.success(), "create_order: {}", stderr(&out));
    let oid = order["id"].as_i64().unwrap().to_string();
    let (invoice, out) = run_json(db, &["create_invoice", &oid]);
    assert!(out.status.success(), "create_invoice: {}", stderr(&out));
    let iid = invoice["id"].as_i64().unwrap();
    let (issued, out) = run_json(db, &["issue_invoice", &iid.to_string()]);
    assert!(out.status.success(), "issue_invoice: {}", stderr(&out));
    assert_eq!(issued["invoice"]["total_cents"], 500_000);
    iid
}

#[test]
fn help_lists_the_whole_command_table() {
    let db = temp_db("help");
    let (help, out) = run_json(&db, &["help"]);
    assert_eq!(out.status.code(), Some(0));
    let names: Vec<&str> = help["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    let expected = [
        "account_ledger",
        "accept_quote",
        "add_characteristic",
        "buy_materials",
        "cancel_order",
        "complete_production",
        "create_component",
        "create_config_model",
        "create_dependency",
        "create_invoice",
        "create_order",
        "create_quote",
        "decline_quote",
        "evaluate_config",
        "expire_quote",
        "fulfil_order",
        "get_config_model",
        "get_entry",
        "get_invoice",
        "get_order",
        "get_quote",
        "health",
        "issue_invoice",
        "issue_materials",
        "list_config_models",
        "list_customers",
        "list_entries",
        "list_invoices",
        "list_items",
        "list_orders",
        "list_payments",
        "list_payments_for_invoice",
        "list_quotes",
        "pay_supplier",
        "post_entry",
        "record_deposit",
        "record_depreciation",
        "record_interest",
        "record_labor",
        "record_payment",
        "record_rent",
        "record_return",
        "send_quote",
        "trial_balance",
    ];
    // Spot-typos in the expected list itself are possible; the real
    // invariant is: every documented command exists exactly once.
    let missing: Vec<&&str> = expected.iter().filter(|e| !names.contains(e)).collect();
    assert!(missing.is_empty(), "help is missing commands: {missing:?}");
    assert_eq!(
        names.len(),
        expected.len(),
        "help lists exactly the command table, no more, no less"
    );
    // create_invoice and issue_invoice are separate commands, always.
    assert!(names.contains(&"create_invoice") && names.contains(&"issue_invoice"));
}

#[test]
fn unknown_commands_and_bad_arguments_exit_1() {
    let db = temp_db("bad-args");
    let out = run(&db, &["frobnicate"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("unknown command 'frobnicate'"));

    let out = run(&db, &["issue_invoice"]);
    assert_eq!(out.status.code(), Some(1), "missing invoice_id is invalid");
    assert!(stderr(&out).contains("'invoice_id'"));

    let out = run(&db, &["issue_invoice", "--json", "not json"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("not valid JSON"));
}

#[test]
fn health_on_a_fresh_database() {
    let db = temp_db("health");
    let (health, out) = run_json(&db, &["health"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(health["status"], "ok");
    assert_eq!(health["accounts"], 22);
    assert_eq!(health["journal_entries"], 1, "the opening entry only");
    assert_eq!(health["config_models"], 1, "the BRK-100 demo model");
}

#[test]
fn refusals_exit_1_with_the_core_message_and_post_nothing() {
    let db = temp_db("refusals");
    let invoice_id = issued_invoice(&db);
    let iid = invoice_id.to_string();

    // (a) Overpayment: $5,000.01 against a $5,000.00 invoice.
    let out = run(
        &db,
        &[
            "record_payment",
            "--invoice-id",
            &iid,
            "--amount-cents",
            "500001",
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("exceeds the balance"),
        "stderr: {}",
        stderr(&out)
    );
    let (health, _) = run_json(&db, &["health"]);
    assert_eq!(
        health["payments"], 0,
        "the refused payment was not recorded"
    );

    // Pay exactly, then a second payment is refused too.
    let (_, out) = run_json(
        &db,
        &[
            "record_payment",
            "--invoice-id",
            &iid,
            "--amount-cents",
            "500000",
        ],
    );
    assert!(out.status.success());
    let out = run(
        &db,
        &[
            "record_payment",
            "--invoice-id",
            &iid,
            "--amount-cents",
            "1",
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("it is 'paid'"));

    // (b) State-machine violation: re-issuing an issued invoice.
    let out = run(&db, &["issue_invoice", &iid]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("would double-post"));

    // (c) post_entry against an unknown account.
    let entries_before = run_json(&db, &["health"]).0["journal_entries"].clone();
    let out = run(
        &db,
        &[
            "post_entry",
            "--json",
            r#"{"entry_date": "2026-10-29", "memo": "bad account", "lines": [{"account_code": "9999", "debit_cents": 100}, {"account_code": "4000", "credit_cents": 100}]}"#,
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("unknown account code: 9999"));
    let (health, _) = run_json(&db, &["health"]);
    assert_eq!(
        health["journal_entries"], entries_before,
        "the refused entry posted nothing"
    );

    // (d) Unbalanced raw entry — refused by the DB trigger, still a
    // plain refusal (exit 1), never an internal error.
    let out = run(
        &db,
        &[
            "post_entry",
            "--json",
            r#"{"entry_date": "2026-10-29", "memo": "unbalanced", "lines": [{"account_code": "1000", "debit_cents": 100}, {"account_code": "4000", "credit_cents": 99}]}"#,
        ],
    );
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
}

#[test]
fn configurator_refusals_carry_the_explanations_on_stderr() {
    let db = temp_db("config-refusals");

    // Aluminum + galvanized violates the finish precondition.
    let out = run(
        &db,
        &[
            "create_quote",
            "--json",
            r#"{"customer_id": 1, "lines": [{"item_id": 1, "qty": 10, "configuration": {"material": "aluminum", "finish": "galvanized"}}]}"#,
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("inconsistent"), "stderr: {err}");
    assert!(
        err.contains("Galvanized finish is available only on steel brackets."),
        "the explanation detail is printed: {err}"
    );
    let (health, _) = run_json(&db, &["health"]);
    assert_eq!(health["quotes"], 0, "the refused quote was not saved");

    // evaluate_config reports the same refusal shape...
    let out = run(
        &db,
        &[
            "evaluate_config",
            "--json",
            r#"{"model_id": 1, "configuration": {"material": "aluminum", "finish": "galvanized"}}"#,
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("Galvanized"));

    // ...while a merely incomplete configuration is a successful
    // answer, not a refusal.
    let (evaluation, out) = run_json(
        &db,
        &[
            "evaluate_config",
            "--json",
            r#"{"model_id": 1, "configuration": {"material": "steel"}}"#,
        ],
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(evaluation["verdict"]["status"], "incomplete");
}
