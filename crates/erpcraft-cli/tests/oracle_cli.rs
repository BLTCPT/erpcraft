//! Day-4 exit test 2: the oracle day, replayed through the CLI door.
//! The engine steps (quote → order → invoice → issue → payment →
//! return) run as CLI commands; the manual postings run through the
//! prep's thin builders (T3/T4/T5/T6/T9/T12/T13/T14) and raw
//! `post_entry` (T2/T7). The ending trial balance must equal the
//! Day-3 oracle PER ACCOUNT — $129,975.00 per side — proving the new
//! door posts exactly what the core posts. (T3 uses $3,600.00, the
//! amount the oracle's own ending balances reconcile to; see the
//! discrepancy note in the core suite.)

use serde_json::Value;
use std::process::{Command, Output};

fn run(db: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_erpcraft"))
        .args(args)
        .env("ERPCRAFT_DB", db)
        .output()
        .expect("the erpcraft binary runs")
}

fn ok(db: &std::path::Path, args: &[&str]) -> Value {
    let out = run(db, args);
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("stdout is JSON")
}

fn customer_id(db: &std::path::Path, name: &str) -> i64 {
    let customers = ok(db, &["list_customers"]);
    customers
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("customer {name} is seeded"))["id"]
        .as_i64()
        .unwrap()
}

/// Quote → send → accept → order → invoice for `qty` plain brackets;
/// returns the draft invoice id (the Day-2/3 flow: drafting posts
/// nothing, issuance posts).
fn draft_invoice(db: &std::path::Path, customer: &str, qty: i64) -> i64 {
    let quote = ok(
        db,
        &[
            "create_quote",
            "--json",
            &serde_json::json!({
                "customer_id": customer_id(db, customer),
                "lines": [{ "item_id": 1, "qty": qty }]
            })
            .to_string(),
        ],
    );
    let qid = quote["id"].as_i64().unwrap().to_string();
    ok(db, &["send_quote", &qid]);
    ok(db, &["accept_quote", &qid]);
    let order = ok(db, &["create_order", &qid]);
    let oid = order["id"].as_i64().unwrap().to_string();
    let invoice = ok(db, &["create_invoice", &oid]);
    invoice["id"].as_i64().unwrap()
}

fn builder(db: &std::path::Path, name: &str, amount_cents: i64) -> Value {
    ok(
        db,
        &[
            name,
            "--amount-cents",
            &amount_cents.to_string(),
            "--entry-date",
            "2026-10-28",
        ],
    )
}

fn raw_entry(db: &std::path::Path, memo: &str, lines: &[(&str, i64, i64)]) -> Value {
    let lines_json: Vec<Value> = lines
        .iter()
        .map(|(code, debit, credit)| {
            serde_json::json!({
                "account_code": code,
                "debit_cents": debit,
                "credit_cents": credit,
            })
        })
        .collect();
    ok(
        db,
        &[
            "post_entry",
            "--json",
            &serde_json::json!({
                "entry_date": "2026-10-28",
                "memo": memo,
                "lines": lines_json,
            })
            .to_string(),
        ],
    )
}

#[test]
fn oracle_day_through_the_cli() {
    let db = std::env::temp_dir().join(format!("erpcraft-oracle-cli-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    let db = db.as_path();

    // Opening state through the door.
    let tb = ok(db, &["trial_balance"]);
    assert_eq!(tb["total_debit_cents"], 12_050_000);
    assert_eq!(tb["total_credit_cents"], 12_050_000);

    // T0+T1: Acme, 40 brackets, issued.
    let acme_invoice = draft_invoice(db, "Acme", 40);
    let issued = ok(db, &["issue_invoice", &acme_invoice.to_string()]);
    assert_eq!(issued["invoice"]["total_cents"], 500_000);
    assert_eq!(issued["revenue_entry"]["lines"][0]["debit_cents"], 500_000);
    assert_eq!(issued["cogs_entry"]["lines"][0]["debit_cents"], 272_000);

    // T2: Beta pays against opening AR — raw post_entry. The door
    // stamps the source; the caller cannot.
    let t2 = raw_entry(
        db,
        "T2 Beta pays against opening AR",
        &[("1000", 620_000, 0), ("1100", 0, 620_000)],
    );
    assert_eq!(t2["source"], "cli");

    // T3–T7 via the builders + raw T7.
    builder(db, "buy_materials", 360_000);
    builder(db, "pay_supplier", 410_000);
    builder(db, "issue_materials", 290_000);
    builder(db, "record_labor", 180_000);
    raw_entry(
        db,
        "T7 production completed",
        &[("1220", 410_000, 0), ("1210", 0, 410_000)],
    );

    // T8: Delta, 25 brackets, issued.
    let delta_invoice = draft_invoice(db, "Delta", 25);
    let issued8 = ok(db, &["issue_invoice", &delta_invoice.to_string()]);
    assert_eq!(issued8["invoice"]["total_cents"], 312_500);
    assert_eq!(issued8["cogs_entry"]["lines"][0]["debit_cents"], 170_000);

    // T9: Epsilon deposit — a liability, not revenue.
    builder(db, "record_deposit", 150_000);

    // T10: Acme pays in full.
    let paid = ok(
        db,
        &[
            "record_payment",
            "--invoice-id",
            &acme_invoice.to_string(),
            "--amount-cents",
            "500000",
        ],
    );
    assert_eq!(paid["payment"]["status"], "recorded");
    let invoice = ok(db, &["get_invoice", &acme_invoice.to_string()]);
    assert_eq!(invoice["status"], "paid");

    // T11: Delta returns 2 brackets.
    let returned = ok(
        db,
        &[
            "record_return",
            "--json",
            &serde_json::json!({
                "invoice_id": delta_invoice,
                "lines": [{ "item_id": 1, "qty": 2 }]
            })
            .to_string(),
        ],
    );
    assert_eq!(returned["revenue_entry"]["lines"][0]["debit_cents"], 25_000);
    assert_eq!(returned["cogs_entry"]["lines"][0]["debit_cents"], 13_600);
    assert_eq!(
        returned["revenue_entry"]["reverses_entry_id"],
        issued8["revenue_entry"]["id"]
    );

    // T12–T14.
    builder(db, "record_rent", 120_000);
    builder(db, "record_interest", 12_500);
    builder(db, "record_depreciation", 35_000);

    // 17 oracle-day entries + the opening entry.
    let health = ok(db, &["health"]);
    assert_eq!(health["journal_entries"], 18);

    // The oracle, per account — the same table the core suite asserts.
    let tb = ok(db, &["trial_balance"]);
    let expected: &[(&str, i64, i64)] = &[
        ("1000", 5_367_500, 0),
        ("1100", 907_500, 0),
        ("1120", 0, 0),
        ("1200", 930_000, 0),
        ("1210", 60_000, 0),
        ("1220", 911_600, 0),
        ("1300", 4_200_000, 0),
        ("1310", 0, 715_000),
        ("2000", 0, 920_000),
        ("2100", 0, 0),
        ("2200", 0, 150_000),
        ("2300", 0, 2_500_000),
        ("2400", 0, 0),
        ("3000", 0, 6_500_000),
        ("3100", 0, 1_400_000),
        ("4000", 0, 812_500),
        ("4010", 25_000, 0),
        ("5000", 428_400, 0),
        ("6000", 120_000, 0),
        ("6010", 12_500, 0),
        ("6020", 35_000, 0),
        ("6030", 0, 0),
    ];
    let rows = tb["rows"].as_array().unwrap();
    assert_eq!(rows.len(), expected.len());
    for (row, (code, debit, credit)) in rows.iter().zip(expected.iter()) {
        assert_eq!(row["code"], *code);
        assert_eq!(row["debit_cents"], *debit, "account {code} debit");
        assert_eq!(row["credit_cents"], *credit, "account {code} credit");
    }
    assert_eq!(tb["total_debit_cents"], 12_997_500, "$129,975.00 per side");
    assert_eq!(tb["total_credit_cents"], 12_997_500);
}
