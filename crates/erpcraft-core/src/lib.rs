//! ERPCraft ledger core.
//!
//! The ledger is the file format: a double-entry journal whose invariants are
//! enforced **in the database**, not just in this API layer:
//!
//! 1. A journal entry may flip from `draft` to `posted` only if it has at
//!    least two lines, a positive total, and sum(debits) == sum(credits)
//!    (enforced by the `trg_entry_post_balance` trigger).
//! 2. Every journal line references a valid account (foreign keys) and is a
//!    one-sided, positive amount (CHECK constraints). Money is integer cents
//!    ([`Cents`]) — never floating point.
//! 3. The journal is append-only: posted entries and their lines cannot be
//!    updated or deleted (triggers). Corrections are new reversing entries
//!    that reference the original via `reverses_entry_id`.
//!
//! On top of the schema sits the seeded demo company, Cascade Precision
//! Parts: a 22-account chart of accounts plus its opening balances posted as
//! a single balanced journal entry (the start-of-day trial balance totals
//! $120,500.00 on both sides).

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::fmt;

pub use rusqlite;

/// Money is always integer minor units (cents). Never f64.
pub type Cents = i64;

pub const DEMO_COMPANY: &str = "Cascade Precision Parts";
/// Standard cost of one finished bracket: $68.00 in cents.
pub const STANDARD_COST_CENTS: Cents = 6_800;
/// Opening trial-balance total on each side: $120,500.00 in cents.
pub const OPENING_TOTAL_CENTS: Cents = 12_050_000;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum LedgerError {
    Db(rusqlite::Error),
    UnknownAccount(String),
    Invalid(String),
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LedgerError::Db(e) => write!(f, "database error: {e}"),
            LedgerError::UnknownAccount(code) => write!(f, "unknown account code: {code}"),
            LedgerError::Invalid(msg) => write!(f, "invalid journal entry: {msg}"),
        }
    }
}

impl std::error::Error for LedgerError {}

impl From<rusqlite::Error> for LedgerError {
    fn from(e: rusqlite::Error) -> Self {
        LedgerError::Db(e)
    }
}

pub type Result<T> = std::result::Result<T, LedgerError>;

// ---------------------------------------------------------------------------
// Data transfer types (shared with the HTTP server)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountView {
    pub code: String,
    pub name: String,
    pub account_type: String,
    /// Signed balance in cents: total debits − total credits over posted entries.
    pub balance_cents: Cents,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrialBalanceRow {
    pub code: String,
    pub name: String,
    pub account_type: String,
    pub debit_cents: Cents,
    pub credit_cents: Cents,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrialBalance {
    pub rows: Vec<TrialBalanceRow>,
    pub total_debit_cents: Cents,
    pub total_credit_cents: Cents,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalLineView {
    pub account_code: String,
    pub debit_cents: Cents,
    pub credit_cents: Cents,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntryView {
    pub id: i64,
    pub entry_date: String,
    pub memo: String,
    pub source: String,
    pub reverses_entry_id: Option<i64>,
    pub status: String,
    pub lines: Vec<JournalLineView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewLine {
    pub account_code: String,
    #[serde(default)]
    pub debit_cents: Cents,
    #[serde(default)]
    pub credit_cents: Cents,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewEntry {
    pub entry_date: String,
    pub memo: String,
    /// "manual" | "agent" | "opening" | "correction" — who/what originated it.
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default)]
    pub reverses_entry_id: Option<i64>,
    pub lines: Vec<NewLine>,
}

fn default_source() -> String {
    "manual".to_string()
}

// ---------------------------------------------------------------------------
// Schema — the double-entry invariant lives here, in the database.
// ---------------------------------------------------------------------------

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
    id           INTEGER PRIMARY KEY,
    code         TEXT NOT NULL UNIQUE,
    name         TEXT NOT NULL,
    account_type TEXT NOT NULL CHECK (account_type IN
        ('asset','contra_asset','liability','equity','revenue','contra_revenue','expense')),
    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS journal_entries (
    id                INTEGER PRIMARY KEY,
    entry_date        TEXT NOT NULL,
    memo              TEXT NOT NULL DEFAULT '',
    source            TEXT NOT NULL DEFAULT 'manual',
    reverses_entry_id INTEGER REFERENCES journal_entries(id),
    status            TEXT NOT NULL DEFAULT 'draft' CHECK (status IN ('draft','posted')),
    created_at        TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS journal_lines (
    id           INTEGER PRIMARY KEY,
    entry_id     INTEGER NOT NULL REFERENCES journal_entries(id),
    account_id   INTEGER NOT NULL REFERENCES accounts(id),
    debit_cents  INTEGER NOT NULL DEFAULT 0 CHECK (debit_cents >= 0),
    credit_cents INTEGER NOT NULL DEFAULT 0 CHECK (credit_cents >= 0),
    CHECK (debit_cents = 0 OR credit_cents = 0),
    CHECK (debit_cents + credit_cents > 0)
);

-- THE INVARIANT: an entry may become 'posted' only if it has >= 2 lines,
-- a positive total, and total debits == total credits. Enforced by SQLite
-- itself, so no code path (API, CLI, MCP, raw SQL) can post an unbalanced
-- entry.
CREATE TRIGGER IF NOT EXISTS trg_entry_post_balance
BEFORE UPDATE OF status ON journal_entries
WHEN NEW.status = 'posted' AND OLD.status = 'draft'
BEGIN
    SELECT RAISE(ABORT, 'journal entry must have at least two lines')
    WHERE (SELECT COUNT(*) FROM journal_lines WHERE entry_id = NEW.id) < 2;
    SELECT RAISE(ABORT, 'unbalanced journal entry: debits must equal credits')
    WHERE (SELECT COALESCE(SUM(debit_cents), 0) FROM journal_lines WHERE entry_id = NEW.id)
       <> (SELECT COALESCE(SUM(credit_cents), 0) FROM journal_lines WHERE entry_id = NEW.id);
    SELECT RAISE(ABORT, 'journal entry total must be positive')
    WHERE (SELECT COALESCE(SUM(debit_cents), 0) FROM journal_lines WHERE entry_id = NEW.id) <= 0;
END;

-- APPEND-ONLY JOURNAL: posted entries and all journal lines are immutable.
CREATE TRIGGER IF NOT EXISTS trg_lines_insert_posted
BEFORE INSERT ON journal_lines
BEGIN
    SELECT RAISE(ABORT, 'cannot add lines to a posted journal entry')
    WHERE (SELECT status FROM journal_entries WHERE id = NEW.entry_id) = 'posted';
END;

CREATE TRIGGER IF NOT EXISTS trg_lines_no_update
BEFORE UPDATE ON journal_lines
BEGIN
    SELECT RAISE(ABORT, 'journal lines are immutable');
END;

CREATE TRIGGER IF NOT EXISTS trg_lines_no_delete
BEFORE DELETE ON journal_lines
BEGIN
    SELECT RAISE(ABORT, 'journal lines are immutable');
END;

CREATE TRIGGER IF NOT EXISTS trg_entries_no_delete
BEFORE DELETE ON journal_entries
BEGIN
    SELECT RAISE(ABORT, 'journal entries are append-only');
END;

CREATE TRIGGER IF NOT EXISTS trg_entries_immutable_when_posted
BEFORE UPDATE ON journal_entries
WHEN OLD.status = 'posted'
BEGIN
    SELECT RAISE(ABORT, 'posted journal entries are immutable');
END;
"#;

// ---------------------------------------------------------------------------
// Seed data — Cascade Precision Parts
// ---------------------------------------------------------------------------

/// The 22-account chart of accounts from the oracle-day spec.
const CHART_OF_ACCOUNTS: &[(&str, &str, &str)] = &[
    ("1000", "Cash", "asset"),
    ("1100", "Accounts Receivable", "asset"),
    ("1120", "Prepaid Insurance", "asset"),
    ("1200", "Raw Materials Inventory", "asset"),
    ("1210", "Work in Process (WIP)", "asset"),
    ("1220", "Finished Goods Inventory", "asset"),
    ("1300", "Equipment", "asset"),
    (
        "1310",
        "Accumulated Depreciation — Equipment",
        "contra_asset",
    ),
    ("2000", "Accounts Payable", "liability"),
    ("2100", "Sales Tax Payable", "liability"),
    ("2200", "Customer Deposits", "liability"),
    ("2300", "Loan Payable", "liability"),
    ("2400", "Wages Payable", "liability"),
    ("3000", "Owner's Equity", "equity"),
    ("3100", "Retained Earnings", "equity"),
    ("4000", "Sales Revenue", "revenue"),
    ("4010", "Sales Returns", "contra_revenue"),
    ("5000", "Cost of Goods Sold", "expense"),
    ("6000", "Rent Expense", "expense"),
    ("6010", "Interest Expense", "expense"),
    ("6020", "Depreciation Expense", "expense"),
    ("6030", "Supplies Expense", "expense"),
];

/// Opening balances (start of day, $120,500.00 per side) as (code, debit, credit).
const OPENING_BALANCES: &[(&str, Cents, Cents)] = &[
    ("1000", 4_820_000, 0), // Cash $48,200.00
    ("1100", 1_240_000, 0), // Accounts Receivable $12,400.00
    ("1200", 860_000, 0),   // Raw Materials $8,600.00
    ("1220", 930_000, 0),   // Finished Goods $9,300.00
    ("1300", 4_200_000, 0), // Equipment $42,000.00
    ("1310", 0, 680_000),   // Accumulated Depreciation $6,800.00
    ("2000", 0, 970_000),   // Accounts Payable $9,700.00
    ("2300", 0, 2_500_000), // Loan Payable $25,000.00
    ("3000", 0, 6_500_000), // Owner's Equity $65,000.00
    ("3100", 0, 1_400_000), // Retained Earnings $14,000.00
];

// ---------------------------------------------------------------------------
// Database setup
// ---------------------------------------------------------------------------

/// Open (or create) a ledger database with foreign keys enforced, create the
/// schema, and seed the demo company if the database is empty.
/// Pass `":memory:"` for an ephemeral database (tests).
pub fn open_db(path: &str) -> Result<Connection> {
    let mut conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    init_schema(&conn)?;
    seed_if_empty(&mut conn)?;
    Ok(conn)
}

/// Create tables + invariant triggers if they do not exist yet.
pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

/// Seed the chart of accounts and the opening-balances entry.
/// Returns true if seeding happened (database was empty).
pub fn seed_if_empty(conn: &mut Connection) -> Result<bool> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))?;
    if count > 0 {
        return Ok(false);
    }
    {
        let tx = conn.transaction()?;
        for (code, name, account_type) in CHART_OF_ACCOUNTS {
            tx.execute(
                "INSERT INTO accounts (code, name, account_type) VALUES (?1, ?2, ?3)",
                params![code, name, account_type],
            )?;
        }
        tx.commit()?;
    }
    // Opening balances are a real journal entry so the full history —
    // including day zero — replays from the append-only journal.
    post_entry(
        conn,
        &NewEntry {
            entry_date: "2026-10-26".to_string(),
            memo: format!("Opening balances — {DEMO_COMPANY}"),
            source: "opening".to_string(),
            reverses_entry_id: None,
            lines: OPENING_BALANCES
                .iter()
                .map(|(code, d, c)| NewLine {
                    account_code: code.to_string(),
                    debit_cents: *d,
                    credit_cents: *c,
                })
                .collect(),
        },
    )?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Ledger operations
// ---------------------------------------------------------------------------

/// Post a journal entry atomically: insert the header as a draft, insert the
/// lines, then flip to `posted` — the `trg_entry_post_balance` trigger
/// validates the double-entry invariant at that moment. Any violation aborts
/// the transaction and nothing persists. Returns the new entry id.
pub fn post_entry(conn: &mut Connection, new: &NewEntry) -> Result<i64> {
    if new.lines.len() < 2 {
        return Err(LedgerError::Invalid(
            "a journal entry needs at least two lines".to_string(),
        ));
    }
    for line in &new.lines {
        if line.debit_cents < 0 || line.credit_cents < 0 {
            return Err(LedgerError::Invalid(
                "amounts may not be negative".to_string(),
            ));
        }
        if line.debit_cents > 0 && line.credit_cents > 0 {
            return Err(LedgerError::Invalid(format!(
                "line for account {} is two-sided (both debit and credit)",
                line.account_code
            )));
        }
        if line.debit_cents + line.credit_cents == 0 {
            return Err(LedgerError::Invalid(format!(
                "line for account {} is zero",
                line.account_code
            )));
        }
    }

    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO journal_entries (entry_date, memo, source, reverses_entry_id, status)
         VALUES (?1, ?2, ?3, ?4, 'draft')",
        params![new.entry_date, new.memo, new.source, new.reverses_entry_id],
    )?;
    let entry_id = tx.last_insert_rowid();
    for line in &new.lines {
        let account_id: i64 = tx
            .query_row(
                "SELECT id FROM accounts WHERE code = ?1",
                params![line.account_code],
                |r| r.get(0),
            )
            .map_err(|_| LedgerError::UnknownAccount(line.account_code.clone()))?;
        tx.execute(
            "INSERT INTO journal_lines (entry_id, account_id, debit_cents, credit_cents)
             VALUES (?1, ?2, ?3, ?4)",
            params![entry_id, account_id, line.debit_cents, line.credit_cents],
        )?;
    }
    // The database itself enforces debits == credits here.
    tx.execute(
        "UPDATE journal_entries SET status = 'posted' WHERE id = ?1",
        params![entry_id],
    )?;
    tx.commit()?;
    Ok(entry_id)
}

/// All accounts with their signed balances (debits − credits) over posted entries.
pub fn list_accounts(conn: &Connection) -> Result<Vec<AccountView>> {
    let mut stmt = conn.prepare(
        "SELECT a.code, a.name, a.account_type,
                COALESCE(SUM(CASE WHEN e.status = 'posted' THEN l.debit_cents ELSE 0 END), 0)
              - COALESCE(SUM(CASE WHEN e.status = 'posted' THEN l.credit_cents ELSE 0 END), 0)
         FROM accounts a
         LEFT JOIN journal_lines l ON l.account_id = a.id
         LEFT JOIN journal_entries e ON e.id = l.entry_id
         GROUP BY a.id
         ORDER BY a.code",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(AccountView {
            code: row.get(0)?,
            name: row.get(1)?,
            account_type: row.get(2)?,
            balance_cents: row.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Trial balance over posted entries: each account's net presented on its
/// debit or credit side; totals must be equal (the oracle assertion).
pub fn trial_balance(conn: &Connection) -> Result<TrialBalance> {
    let accounts = list_accounts(conn)?;
    let mut rows = Vec::new();
    let mut total_debit: Cents = 0;
    let mut total_credit: Cents = 0;
    for a in accounts {
        let (d, c) = if a.balance_cents >= 0 {
            (a.balance_cents, 0)
        } else {
            (0, -a.balance_cents)
        };
        total_debit += d;
        total_credit += c;
        rows.push(TrialBalanceRow {
            code: a.code,
            name: a.name,
            account_type: a.account_type,
            debit_cents: d,
            credit_cents: c,
        });
    }
    Ok(TrialBalance {
        rows,
        total_debit_cents: total_debit,
        total_credit_cents: total_credit,
    })
}

/// Fetch one posted (or draft) entry with its lines, by id.
pub fn get_entry(conn: &Connection, entry_id: i64) -> Result<Option<JournalEntryView>> {
    let mut entries = fetch_entries(conn, Some(entry_id))?;
    Ok(entries.pop())
}

/// List all journal entries (oldest first) with their lines.
pub fn list_entries(conn: &Connection) -> Result<Vec<JournalEntryView>> {
    fetch_entries(conn, None)
}

fn fetch_entries(conn: &Connection, only_id: Option<i64>) -> Result<Vec<JournalEntryView>> {
    let mut stmt = conn.prepare(
        "SELECT id, entry_date, memo, source, reverses_entry_id, status
         FROM journal_entries
         WHERE (?1 IS NULL OR id = ?1)
         ORDER BY id",
    )?;
    let entry_rows = stmt.query_map(params![only_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    let mut entries = Vec::new();
    for r in entry_rows {
        let (id, entry_date, memo, source, reverses_entry_id, status) = r?;
        entries.push(JournalEntryView {
            id,
            entry_date,
            memo,
            source,
            reverses_entry_id,
            status,
            lines: Vec::new(),
        });
    }
    let mut line_stmt = conn.prepare(
        "SELECT a.code, l.debit_cents, l.credit_cents
         FROM journal_lines l
         JOIN accounts a ON a.id = l.account_id
         WHERE l.entry_id = ?1
         ORDER BY l.id",
    )?;
    for entry in entries.iter_mut() {
        let lines = line_stmt.query_map(params![entry.id], |row| {
            Ok(JournalLineView {
                account_code: row.get(0)?,
                debit_cents: row.get(1)?,
                credit_cents: row.get(2)?,
            })
        })?;
        for l in lines {
            entry.lines.push(l?);
        }
    }
    Ok(entries)
}

/// Count of journal entries (any status).
pub fn entry_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM journal_entries", [], |r| r.get(0))?)
}

/// Count of seeded accounts.
pub fn account_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))?)
}
