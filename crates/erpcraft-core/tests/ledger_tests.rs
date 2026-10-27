//! Day-1 invariant tests for the ERPCraft ledger core.
//!
//! The point of these tests is not that the Rust code is careful — it is that
//! the *database* refuses bad postings, so no future code path (HTTP, CLI,
//! MCP, an AI agent, raw SQL) can break the ledger.

use erpcraft_core::{
    account_count, entry_count, get_entry, list_accounts, open_db, post_entry, trial_balance,
    NewEntry, NewLine, OPENING_TOTAL_CENTS,
};

fn line(code: &str, debit: i64, credit: i64) -> NewLine {
    NewLine {
        account_code: code.to_string(),
        debit_cents: debit,
        credit_cents: credit,
    }
}

fn entry(lines: Vec<NewLine>) -> NewEntry {
    NewEntry {
        entry_date: "2026-10-26".to_string(),
        memo: "test entry".to_string(),
        source: "manual".to_string(),
        reverses_entry_id: None,
        lines,
    }
}

#[test]
fn seed_creates_the_22_account_coa() {
    let conn = open_db(":memory:").unwrap();
    assert_eq!(account_count(&conn).unwrap(), 22);
    let accounts = list_accounts(&conn).unwrap();
    let codes: Vec<&str> = accounts.iter().map(|a| a.code.as_str()).collect();
    assert_eq!(
        codes,
        vec![
            "1000", "1100", "1120", "1200", "1210", "1220", "1300", "1310", "2000", "2100", "2200",
            "2300", "2400", "3000", "3100", "4000", "4010", "5000", "6000", "6010", "6020", "6030"
        ]
    );
}

#[test]
fn opening_trial_balance_ties_out() {
    let conn = open_db(":memory:").unwrap();
    let tb = trial_balance(&conn).unwrap();
    // $120,500.00 on both sides, per the oracle-day spec.
    assert_eq!(tb.total_debit_cents, OPENING_TOTAL_CENTS);
    assert_eq!(tb.total_credit_cents, OPENING_TOTAL_CENTS);
    assert_eq!(tb.total_debit_cents, 12_050_000);
    // Spot-check two accounts against the spec.
    let cash = tb.rows.iter().find(|r| r.code == "1000").unwrap();
    assert_eq!(cash.debit_cents, 4_820_000); // $48,200.00
    let accum = tb.rows.iter().find(|r| r.code == "1310").unwrap();
    assert_eq!(accum.credit_cents, 680_000); // $6,800.00 contra-asset
}

#[test]
fn balanced_manual_posting_round_trips() {
    let mut conn = open_db(":memory:").unwrap();
    // A manual sale on account: Dr AR $100.00 / Cr Sales Revenue $100.00.
    let id = post_entry(
        &mut conn,
        &entry(vec![line("1100", 10_000, 0), line("4000", 0, 10_000)]),
    )
    .unwrap();

    // Read it back exactly as posted.
    let posted = get_entry(&conn, id).unwrap().expect("entry exists");
    assert_eq!(posted.status, "posted");
    assert_eq!(posted.lines.len(), 2);
    assert_eq!(posted.lines[0].account_code, "1100");
    assert_eq!(posted.lines[0].debit_cents, 10_000);
    assert_eq!(posted.lines[1].account_code, "4000");
    assert_eq!(posted.lines[1].credit_cents, 10_000);

    // Balances tie out: both sides grew by the $100.00 posting.
    let tb = trial_balance(&conn).unwrap();
    assert_eq!(tb.total_debit_cents, OPENING_TOTAL_CENTS + 10_000);
    assert_eq!(tb.total_credit_cents, OPENING_TOTAL_CENTS + 10_000);
    let ar = tb.rows.iter().find(|r| r.code == "1100").unwrap();
    assert_eq!(ar.debit_cents, 1_240_000 + 10_000);
}

#[test]
fn unbalanced_posting_is_rejected_and_nothing_persists() {
    let mut conn = open_db(":memory:").unwrap();
    let before = entry_count(&conn).unwrap();

    // Dr Cash $100.00 / Cr Sales Revenue $99.00 — one cent short.
    let result = post_entry(
        &mut conn,
        &entry(vec![line("1000", 10_000, 0), line("4000", 0, 9_900)]),
    );
    let err = result.expect_err("unbalanced entry must be rejected");
    assert!(
        err.to_string().contains("debits must equal credits"),
        "rejection must come from the DB invariant trigger, got: {err}"
    );

    // The whole transaction rolled back: no entry, balances untouched.
    assert_eq!(entry_count(&conn).unwrap(), before);
    let tb = trial_balance(&conn).unwrap();
    assert_eq!(tb.total_debit_cents, OPENING_TOTAL_CENTS);
    assert_eq!(tb.total_credit_cents, OPENING_TOTAL_CENTS);
}

#[test]
fn unbalanced_posting_via_raw_sql_is_also_rejected() {
    // Bypass the Rust API entirely: hand-craft a draft, add lines, try to
    // flip it to posted with raw SQL. The database must still refuse.
    let conn = open_db(":memory:").unwrap();
    conn.execute(
        "INSERT INTO journal_entries (entry_date, memo, source, status)
         VALUES ('2026-10-26', 'raw sql attack', 'manual', 'draft')",
        [],
    )
    .unwrap();
    let id = conn.last_insert_rowid();
    let cash_id: i64 = conn
        .query_row("SELECT id FROM accounts WHERE code = '1000'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let rev_id: i64 = conn
        .query_row("SELECT id FROM accounts WHERE code = '4000'", [], |r| {
            r.get(0)
        })
        .unwrap();
    conn.execute(
        "INSERT INTO journal_lines (entry_id, account_id, debit_cents, credit_cents)
         VALUES (?1, ?2, 5000, 0)",
        [id, cash_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO journal_lines (entry_id, account_id, debit_cents, credit_cents)
         VALUES (?1, ?2, 0, 4999)",
        [id, rev_id],
    )
    .unwrap();
    let flip = conn.execute(
        "UPDATE journal_entries SET status = 'posted' WHERE id = ?1",
        [id],
    );
    assert!(flip.is_err(), "raw SQL must not be able to post unbalanced");
}

#[test]
fn posted_journal_is_immutable_at_the_db_level() {
    let conn = open_db(":memory:").unwrap();
    // The opening entry is posted (id 1 in a fresh database).
    let update_line = conn.execute("UPDATE journal_lines SET debit_cents = 1 WHERE id = 1", []);
    assert!(update_line.is_err(), "posted lines cannot be edited");
    let delete_line = conn.execute("DELETE FROM journal_lines WHERE id = 1", []);
    assert!(delete_line.is_err(), "posted lines cannot be deleted");
    let delete_entry = conn.execute("DELETE FROM journal_entries WHERE id = 1", []);
    assert!(delete_entry.is_err(), "entries cannot be deleted");
    let add_line = conn.execute(
        "INSERT INTO journal_lines (entry_id, account_id, debit_cents, credit_cents)
         SELECT 1, id, 100, 0 FROM accounts WHERE code = '1000'",
        [],
    );
    assert!(add_line.is_err(), "cannot add lines to a posted entry");
    let edit_entry = conn.execute(
        "UPDATE journal_entries SET memo = 'rewritten' WHERE id = 1",
        [],
    );
    assert!(edit_entry.is_err(), "posted entries cannot be edited");
}

#[test]
fn line_referencing_unknown_account_is_rejected() {
    let mut conn = open_db(":memory:").unwrap();
    let before = entry_count(&conn).unwrap();
    let result = post_entry(
        &mut conn,
        &entry(vec![line("1000", 10_000, 0), line("9999", 0, 10_000)]),
    );
    assert!(result.is_err(), "unknown account code must be rejected");
    assert_eq!(entry_count(&conn).unwrap(), before);
}

#[test]
fn degenerate_entries_are_rejected() {
    let mut conn = open_db(":memory:").unwrap();
    // Single line.
    assert!(post_entry(&mut conn, &entry(vec![line("1000", 10_000, 0)])).is_err());
    // Two-sided line.
    assert!(post_entry(
        &mut conn,
        &entry(vec![line("1000", 10_000, 10_000), line("4000", 0, 10_000)])
    )
    .is_err());
    // Zero line.
    assert!(post_entry(
        &mut conn,
        &entry(vec![line("1000", 0, 0), line("4000", 0, 10_000)])
    )
    .is_err());
    assert_eq!(
        entry_count(&conn).unwrap(),
        1,
        "only the opening entry exists"
    );
}
