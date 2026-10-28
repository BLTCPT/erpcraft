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
use std::collections::BTreeMap;
use std::fmt;

pub use rusqlite;

pub mod configurator;

pub use configurator::{
    add_characteristic, config_model_count, config_model_for_item, create_component,
    create_config_model, create_dependency, dependency_fingerprint, evaluate,
    evaluate_configuration, get_config_model, list_config_models, seed_configurator_if_empty,
    CharacteristicValueView, CharacteristicView, CmpOp, ComponentView, ConfigModelView,
    ConfigValue, DependencyView, Evaluation, Explanation, Expr, ForcedComponent, ForcedValue,
    MessageParts, NewCharacteristic, NewCharacteristicValue, NewComponent, NewConfigModel,
    NewDependency, Verdict,
};

/// Money is always integer minor units (cents). Never f64.
pub type Cents = i64;

pub const DEMO_COMPANY: &str = "Cascade Precision Parts";
/// Standard cost of one finished bracket: $68.00 in cents.
pub const STANDARD_COST_CENTS: Cents = 6_800;
/// Unit selling price of one finished bracket: $125.00 in cents.
/// (The oracle day sells 40 units for $5,000.00 of revenue — see DAY3_PREP.)
pub const BRACKET_UNIT_PRICE_CENTS: Cents = 12_500;
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
    /// A variant-configuration rejection: carries the evaluator's
    /// structured explanations so every surface can render the reasons,
    /// not just a message.
    Configuration {
        message: String,
        explanations: Vec<configurator::Explanation>,
    },
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LedgerError::Db(e) => write!(f, "database error: {e}"),
            LedgerError::UnknownAccount(code) => write!(f, "unknown account code: {code}"),
            LedgerError::Invalid(msg) => write!(f, "invalid journal entry: {msg}"),
            LedgerError::Configuration { message, .. } => write!(f, "{message}"),
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

-- ---------------------------------------------------------------------------
-- Day 2: order-to-cash documents. These tables NEVER touch the journal —
-- GL recognition for document events is Day-3 work. Status values are
-- pinned by CHECK constraints; the legal *transitions* between them are
-- enforced by the core API (state machines in the order-to-cash section).
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS customers (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS items (
    id                  INTEGER PRIMARY KEY,
    sku                 TEXT NOT NULL UNIQUE,
    name                TEXT NOT NULL,
    unit_price_cents    INTEGER NOT NULL CHECK (unit_price_cents >= 0),
    standard_cost_cents INTEGER NOT NULL CHECK (standard_cost_cents >= 0),
    created_at          TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS quotes (
    id          INTEGER PRIMARY KEY,
    customer_id INTEGER NOT NULL REFERENCES customers(id),
    status      TEXT NOT NULL DEFAULT 'draft'
                CHECK (status IN ('draft','sent','accepted','declined','expired')),
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS quote_lines (
    id                  INTEGER PRIMARY KEY,
    quote_id            INTEGER NOT NULL REFERENCES quotes(id),
    item_id             INTEGER NOT NULL REFERENCES items(id),
    qty                 INTEGER NOT NULL CHECK (qty > 0),
    unit_price_cents    INTEGER NOT NULL CHECK (unit_price_cents >= 0),
    standard_cost_cents INTEGER,
    configuration       TEXT
);

CREATE TABLE IF NOT EXISTS sales_orders (
    id          INTEGER PRIMARY KEY,
    quote_id    INTEGER NOT NULL UNIQUE REFERENCES quotes(id),
    customer_id INTEGER NOT NULL REFERENCES customers(id),
    status      TEXT NOT NULL DEFAULT 'open'
                CHECK (status IN ('open','fulfilled','cancelled')),
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS order_lines (
    id                  INTEGER PRIMARY KEY,
    order_id            INTEGER NOT NULL REFERENCES sales_orders(id),
    item_id             INTEGER NOT NULL REFERENCES items(id),
    qty                 INTEGER NOT NULL CHECK (qty > 0),
    unit_price_cents    INTEGER NOT NULL CHECK (unit_price_cents >= 0),
    standard_cost_cents INTEGER,
    configuration       TEXT
);

CREATE TABLE IF NOT EXISTS invoices (
    id          INTEGER PRIMARY KEY,
    order_id    INTEGER NOT NULL UNIQUE REFERENCES sales_orders(id),
    customer_id INTEGER NOT NULL REFERENCES customers(id),
    status      TEXT NOT NULL DEFAULT 'draft'
                CHECK (status IN ('draft','issued','partially_paid','paid','void')),
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS invoice_lines (
    id                  INTEGER PRIMARY KEY,
    invoice_id          INTEGER NOT NULL REFERENCES invoices(id),
    item_id             INTEGER NOT NULL REFERENCES items(id),
    qty                 INTEGER NOT NULL CHECK (qty > 0),
    unit_price_cents    INTEGER NOT NULL CHECK (unit_price_cents >= 0),
    standard_cost_cents INTEGER,
    configuration       TEXT
);

CREATE TABLE IF NOT EXISTS payments (
    id           INTEGER PRIMARY KEY,
    invoice_id   INTEGER NOT NULL REFERENCES invoices(id),
    amount_cents INTEGER NOT NULL CHECK (amount_cents > 0),
    status       TEXT NOT NULL DEFAULT 'recorded' CHECK (status IN ('recorded')),
    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
);

-- ---------------------------------------------------------------------------
-- Variant configurator (CONFIGURATOR_DESIGN.md). Dependencies are typed
-- expression trees stored as JSON — never source text. The evaluator in
-- the `configurator` module is pure; these tables are its only state.
-- A quote line's `configuration` column snapshots the evaluated
-- configuration plus its rolled-up pricing, so later dependency edits
-- never rewrite a quoted line.
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS config_models (
    id         INTEGER PRIMARY KEY,
    item_id    INTEGER NOT NULL UNIQUE REFERENCES items(id),
    name       TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS characteristics (
    id         INTEGER PRIMARY KEY,
    model_id   INTEGER NOT NULL REFERENCES config_models(id),
    code       TEXT NOT NULL,
    name       TEXT NOT NULL,
    value_type TEXT NOT NULL CHECK (value_type IN ('enum','integer','bool')),
    min_value  INTEGER,
    max_value  INTEGER,
    required   INTEGER NOT NULL DEFAULT 1 CHECK (required IN (0,1)),
    UNIQUE (model_id, code)
);

CREATE TABLE IF NOT EXISTS characteristic_values (
    id                INTEGER PRIMARY KEY,
    characteristic_id INTEGER NOT NULL REFERENCES characteristics(id),
    code              TEXT NOT NULL,
    label             TEXT NOT NULL,
    UNIQUE (characteristic_id, code)
);

CREATE TABLE IF NOT EXISTS dependencies (
    id          INTEGER PRIMARY KEY,
    model_id    INTEGER NOT NULL REFERENCES config_models(id),
    kind        TEXT NOT NULL CHECK (kind IN ('precondition','selection','constraint')),
    target      TEXT NOT NULL,
    expr        TEXT NOT NULL,
    source_text TEXT NOT NULL DEFAULT '',
    created_by  TEXT NOT NULL DEFAULT 'api',
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS components (
    id                INTEGER PRIMARY KEY,
    model_id          INTEGER NOT NULL REFERENCES config_models(id),
    code              TEXT NOT NULL,
    name              TEXT NOT NULL,
    component_item_id INTEGER REFERENCES items(id),
    qty               INTEGER NOT NULL DEFAULT 1 CHECK (qty > 0),
    price_delta_cents INTEGER NOT NULL DEFAULT 0,
    cost_delta_cents  INTEGER NOT NULL DEFAULT 0,
    UNIQUE (model_id, code)
);
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
    seed_master_data_if_empty(&conn)?;
    seed_configurator_if_empty(&conn)?;
    Ok(conn)
}

/// Create tables + invariant triggers if they do not exist yet.
pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    // Column migrations for databases created before the configurator:
    // CREATE TABLE IF NOT EXISTS cannot add columns to an existing
    // table, so the line tables gain their snapshot columns here.
    for table in ["quote_lines", "order_lines", "invoice_lines"] {
        ensure_column(conn, table, "standard_cost_cents", "INTEGER")?;
        ensure_column(conn, table, "configuration", "TEXT")?;
    }
    Ok(())
}

/// Add a column to a table if it is not there yet. Table and column
/// names are internal constants, never caller input.
fn ensure_column(conn: &Connection, table: &str, column: &str, ddl: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for c in cols {
        if c? == column {
            return Ok(());
        }
    }
    conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {ddl}"),
        [],
    )?;
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

// ---------------------------------------------------------------------------
// Day 2: order-to-cash — demo master data (customers, items)
// ---------------------------------------------------------------------------

/// The four frozen demo customers of Cascade Precision Parts.
const DEMO_CUSTOMERS: &[&str] = &["Acme", "Beta", "Delta", "Epsilon"];

/// SKU of the one seeded finished good.
pub const BRACKET_SKU: &str = "BRK-100";
/// Name of the one seeded finished good.
pub const BRACKET_NAME: &str = "Finished Bracket";

/// Seed the demo master data: the four customers and the finished bracket.
/// Returns true if seeding happened (the customers table was empty).
/// Runs independently of the GL seed so a database created on Day 1 gains
/// its master data on the next open, and re-opening is a no-op.
pub fn seed_master_data_if_empty(conn: &Connection) -> Result<bool> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM customers", [], |r| r.get(0))?;
    if count > 0 {
        return Ok(false);
    }
    for name in DEMO_CUSTOMERS {
        conn.execute("INSERT INTO customers (name) VALUES (?1)", params![name])?;
    }
    conn.execute(
        "INSERT INTO items (sku, name, unit_price_cents, standard_cost_cents)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            BRACKET_SKU,
            BRACKET_NAME,
            BRACKET_UNIT_PRICE_CENTS,
            STANDARD_COST_CENTS
        ],
    )?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Day 2: order-to-cash — data transfer types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomerView {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemView {
    pub id: i64,
    pub sku: String,
    pub name: String,
    pub unit_price_cents: Cents,
    pub standard_cost_cents: Cents,
}

/// One document line. `line_total_cents` is always derived
/// (qty × unit_price_cents), never stored and never client-supplied.
/// `standard_cost_cents` is the per-unit cost snapshotted at quote time
/// (for configured lines it already includes the selected components'
/// cost deltas), and `configuration` is the variant-configuration
/// snapshot for configured lines — both copied unchanged from quote to
/// order to invoice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocLineView {
    pub item_id: i64,
    pub sku: String,
    pub qty: i64,
    pub unit_price_cents: Cents,
    pub line_total_cents: Cents,
    pub standard_cost_cents: Option<Cents>,
    pub configuration: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteView {
    pub id: i64,
    pub customer_id: i64,
    pub customer_name: String,
    pub status: String,
    pub lines: Vec<DocLineView>,
    /// Derived: sum of the line totals.
    pub total_cents: Cents,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderView {
    pub id: i64,
    /// Source document: the accepted quote this order was created from.
    pub quote_id: i64,
    pub customer_id: i64,
    pub customer_name: String,
    pub status: String,
    pub lines: Vec<DocLineView>,
    /// Derived: sum of the line totals.
    pub total_cents: Cents,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvoiceView {
    pub id: i64,
    /// Source document: the sales order this invoice was created from.
    pub order_id: i64,
    pub customer_id: i64,
    pub customer_name: String,
    pub status: String,
    pub lines: Vec<DocLineView>,
    /// Derived: sum of the line totals.
    pub total_cents: Cents,
    /// Derived: sum of recorded payments against this invoice.
    pub paid_cents: Cents,
    /// Derived: total − paid.
    pub balance_cents: Cents,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentView {
    pub id: i64,
    /// Source document: the invoice this payment is recorded against.
    pub invoice_id: i64,
    pub amount_cents: Cents,
    pub status: String,
}

/// One line of a new quote: which item and how many. The unit price is
/// snapshotted from the item master when the quote is created — it is
/// never supplied by the caller. A line may carry a variant
/// `configuration` (characteristic code → value); configured lines are
/// evaluated and priced by the configurator — see [`price_quote_line`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewQuoteLine {
    pub item_id: i64,
    pub qty: i64,
    #[serde(default)]
    pub configuration: Option<BTreeMap<String, ConfigValue>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewQuote {
    pub customer_id: i64,
    pub lines: Vec<NewQuoteLine>,
}

// ---------------------------------------------------------------------------
// Day 2: order-to-cash — master data reads
// ---------------------------------------------------------------------------

/// All customers, in seed order.
pub fn list_customers(conn: &Connection) -> Result<Vec<CustomerView>> {
    let mut stmt = conn.prepare("SELECT id, name FROM customers ORDER BY id")?;
    let rows = stmt.query_map([], |row| {
        Ok(CustomerView {
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

/// All sellable items.
pub fn list_items(conn: &Connection) -> Result<Vec<ItemView>> {
    let mut stmt = conn.prepare(
        "SELECT id, sku, name, unit_price_cents, standard_cost_cents
         FROM items ORDER BY id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(ItemView {
            id: row.get(0)?,
            sku: row.get(1)?,
            name: row.get(2)?,
            unit_price_cents: row.get(3)?,
            standard_cost_cents: row.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Day 2: order-to-cash — document internals
// ---------------------------------------------------------------------------

/// Map a missing-row read to a plain-language `Invalid`; pass real
/// database errors through untouched.
fn missing(what: &str, id: i64) -> impl Fn(rusqlite::Error) -> LedgerError {
    let what = what.to_string();
    move |e| match e {
        rusqlite::Error::QueryReturnedNoRows => {
            LedgerError::Invalid(format!("no {what} with id {id}"))
        }
        other => LedgerError::Db(other),
    }
}

fn doc_lines(
    conn: &Connection,
    lines_table: &str,
    fk_column: &str,
    doc_id: i64,
) -> Result<Vec<DocLineView>> {
    let sql = format!(
        "SELECT l.item_id, i.sku, l.qty, l.unit_price_cents, l.standard_cost_cents, l.configuration
         FROM {lines_table} l
         JOIN items i ON i.id = l.item_id
         WHERE l.{fk_column} = ?1
         ORDER BY l.id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![doc_id], |row| {
        let qty: i64 = row.get(2)?;
        let unit_price_cents: Cents = row.get(3)?;
        let configuration_json: Option<String> = row.get(5)?;
        Ok((
            DocLineView {
                item_id: row.get(0)?,
                sku: row.get(1)?,
                qty,
                unit_price_cents,
                line_total_cents: qty * unit_price_cents,
                standard_cost_cents: row.get(4)?,
                configuration: None, // parsed below, outside the row closure
            },
            configuration_json,
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (mut line, configuration_json) = r?;
        if let Some(json) = configuration_json {
            line.configuration = Some(serde_json::from_str(&json).map_err(|_| {
                LedgerError::Invalid("a stored line configuration is unreadable".to_string())
            })?);
        }
        out.push(line);
    }
    Ok(out)
}

fn lines_total(lines: &[DocLineView]) -> Cents {
    lines.iter().map(|l| l.line_total_cents).sum()
}

fn fetch_quote(conn: &Connection, quote_id: i64) -> Result<QuoteView> {
    let (customer_id, customer_name, status) = conn
        .query_row(
            "SELECT q.customer_id, c.name, q.status
             FROM quotes q JOIN customers c ON c.id = q.customer_id
             WHERE q.id = ?1",
            params![quote_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(missing("quote", quote_id))?;
    let lines = doc_lines(conn, "quote_lines", "quote_id", quote_id)?;
    let total_cents = lines_total(&lines);
    Ok(QuoteView {
        id: quote_id,
        customer_id,
        customer_name,
        status,
        lines,
        total_cents,
    })
}

fn fetch_order(conn: &Connection, order_id: i64) -> Result<OrderView> {
    let (quote_id, customer_id, customer_name, status) = conn
        .query_row(
            "SELECT o.quote_id, o.customer_id, c.name, o.status
             FROM sales_orders o JOIN customers c ON c.id = o.customer_id
             WHERE o.id = ?1",
            params![order_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .map_err(missing("sales order", order_id))?;
    let lines = doc_lines(conn, "order_lines", "order_id", order_id)?;
    let total_cents = lines_total(&lines);
    Ok(OrderView {
        id: order_id,
        quote_id,
        customer_id,
        customer_name,
        status,
        lines,
        total_cents,
    })
}

fn fetch_invoice(conn: &Connection, invoice_id: i64) -> Result<InvoiceView> {
    let (order_id, customer_id, customer_name, status) = conn
        .query_row(
            "SELECT i.order_id, i.customer_id, c.name, i.status
             FROM invoices i JOIN customers c ON c.id = i.customer_id
             WHERE i.id = ?1",
            params![invoice_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .map_err(missing("invoice", invoice_id))?;
    let lines = doc_lines(conn, "invoice_lines", "invoice_id", invoice_id)?;
    let total_cents = lines_total(&lines);
    let paid_cents: Cents = conn.query_row(
        "SELECT COALESCE(SUM(amount_cents), 0) FROM payments WHERE invoice_id = ?1",
        params![invoice_id],
        |r| r.get(0),
    )?;
    Ok(InvoiceView {
        id: invoice_id,
        order_id,
        customer_id,
        customer_name,
        status,
        lines,
        total_cents,
        paid_cents,
        balance_cents: total_cents - paid_cents,
    })
}

/// One legal state-machine step: the states it may start from, the state
/// it lands in, and the wording used when it is refused.
struct Transition {
    from: &'static [&'static str],
    to: &'static str,
    action: &'static str,
    hint: &'static str,
}

/// Shared state-machine step for quotes, orders and invoices: read the
/// current status, refuse anything that is not a legal `from` state with
/// a plain-language reason, then write the new status.
fn transition(conn: &Connection, table: &str, what: &str, id: i64, step: Transition) -> Result<()> {
    let status: String = conn
        .query_row(
            &format!("SELECT status FROM {table} WHERE id = ?1"),
            params![id],
            |r| r.get(0),
        )
        .map_err(missing(what, id))?;
    if !step.from.contains(&status.as_str()) {
        return Err(LedgerError::Invalid(format!(
            "cannot {} {what} {id}: it is '{status}' — {}",
            step.action, step.hint
        )));
    }
    conn.execute(
        &format!("UPDATE {table} SET status = ?1 WHERE id = ?2"),
        params![step.to, id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Day 2: order-to-cash — quotes
// ---------------------------------------------------------------------------

/// Price one new quote line: returns (unit_price_cents,
/// standard_cost_cents, configuration snapshot JSON).
///
/// Unconfigured lines price exactly as Day 2 priced them — the item
/// master's price and standard cost, snapshotted. Configured lines go
/// through the configurator (CONFIGURATOR_DESIGN.md, "Pricing"): the
/// configuration is evaluated, and the item's base price/cost are
/// rolled up with the price/cost deltas of the components the
/// evaluation selected. The evaluator itself never touches prices —
/// this step consumes its selected/forced output. An inconsistent
/// configuration is refused here with the evaluator's explanations; an
/// incomplete one is allowed while the quote is a draft (acceptance
/// demands consistency — see [`accept_quote`]). A computed price or
/// cost below zero is a constraint violation, never silently clamped.
fn price_quote_line(
    conn: &Connection,
    line: &NewQuoteLine,
) -> Result<(Cents, Cents, Option<String>)> {
    let (base_price, base_cost) = conn
        .query_row(
            "SELECT unit_price_cents, standard_cost_cents FROM items WHERE id = ?1",
            params![line.item_id],
            |r| Ok((r.get::<_, Cents>(0)?, r.get::<_, Cents>(1)?)),
        )
        .map_err(|_| LedgerError::Invalid(format!("unknown item id {}", line.item_id)))?;
    let Some(config) = &line.configuration else {
        return Ok((base_price, base_cost, None));
    };

    let model = config_model_for_item(conn, line.item_id)?.ok_or_else(|| {
        LedgerError::Invalid(format!(
            "item {} has no variant configuration model",
            line.item_id
        ))
    })?;

    // The line quantity *is* the quantity characteristic when the model
    // defines one; a caller-supplied quantity must agree with it.
    let mut eval_input = config.clone();
    if model.characteristic("quantity").is_some() {
        if let Some(given) = eval_input.get("quantity") {
            if *given != ConfigValue::Int(line.qty) {
                return Err(LedgerError::Invalid(format!(
                    "configuration quantity {} does not match the line quantity {}",
                    given.render(),
                    line.qty
                )));
            }
        }
        eval_input.insert("quantity".to_string(), ConfigValue::Int(line.qty));
    }

    let evaluation = evaluate(&model, &eval_input);
    if let Verdict::Inconsistent = evaluation.verdict {
        return Err(LedgerError::Configuration {
            message: format!(
                "the configuration for item {} is inconsistent: {}",
                line.item_id,
                evaluation
                    .explanations
                    .first()
                    .map(|e| e.message_parts.detail.clone())
                    .unwrap_or_else(|| "no explanation recorded".to_string())
            ),
            explanations: evaluation.explanations.clone(),
        });
    }

    // Roll up: base + Σ deltas of the selected components.
    let mut unit_price_cents = base_price;
    let mut standard_cost_cents = base_cost;
    for component in &model.components {
        if evaluation
            .selected_components
            .iter()
            .any(|c| c == &component.code)
        {
            unit_price_cents += component.price_delta_cents * component.qty;
            standard_cost_cents += component.cost_delta_cents * component.qty;
        }
    }
    if unit_price_cents < 0 || standard_cost_cents < 0 {
        let detail = format!(
            "the configuration prices the line at {unit_price_cents} cents with a standard cost of {standard_cost_cents} cents — a configured line may not price below zero"
        );
        return Err(LedgerError::Configuration {
            message: detail.clone(),
            explanations: vec![Explanation {
                dependency_id: None,
                kind: "constraint".to_string(),
                message_parts: MessageParts {
                    characteristics: Vec::new(),
                    values: Vec::new(),
                    detail,
                },
            }],
        });
    }

    // Snapshot: the effective values (chosen + forced), what was
    // selected, the verdict, the dependency fingerprint it was
    // evaluated against, and the rolled-up pricing. Frozen here —
    // later dependency edits never rewrite it.
    let mut values = eval_input;
    for (code, forced) in &evaluation.forced {
        values.insert(code.clone(), forced.value.clone());
    }
    let verdict = match &evaluation.verdict {
        Verdict::Consistent => "consistent",
        Verdict::Incomplete { .. } => "incomplete",
        Verdict::Inconsistent => "inconsistent",
    };
    let snapshot = serde_json::json!({
        "model_id": model.id,
        "dependency_fingerprint": dependency_fingerprint(&model),
        "values": values,
        "forced": evaluation.forced,
        "selected_components": evaluation.selected_components,
        "verdict": verdict,
        "unit_price_cents": unit_price_cents,
        "standard_cost_cents": standard_cost_cents,
    });
    Ok((
        unit_price_cents,
        standard_cost_cents,
        Some(snapshot.to_string()),
    ))
}

/// Create a quote in `draft`. Line prices are snapshotted from the item
/// master; totals are derived from the lines from then on. Configured
/// lines are evaluated and priced by [`price_quote_line`].
pub fn create_quote(conn: &mut Connection, new: &NewQuote) -> Result<i64> {
    if new.lines.is_empty() {
        return Err(LedgerError::Invalid(
            "a quote needs at least one line".to_string(),
        ));
    }
    for line in &new.lines {
        if line.qty <= 0 {
            return Err(LedgerError::Invalid(
                "quote line quantities must be positive".to_string(),
            ));
        }
    }
    let tx = conn.transaction()?;
    let customer_known: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM customers WHERE id = ?1)",
        params![new.customer_id],
        |r| r.get(0),
    )?;
    if !customer_known {
        return Err(LedgerError::Invalid(format!(
            "unknown customer id {}",
            new.customer_id
        )));
    }
    tx.execute(
        "INSERT INTO quotes (customer_id, status) VALUES (?1, 'draft')",
        params![new.customer_id],
    )?;
    let quote_id = tx.last_insert_rowid();
    for line in &new.lines {
        let (unit_price_cents, standard_cost_cents, configuration) = price_quote_line(&tx, line)?;
        tx.execute(
            "INSERT INTO quote_lines (quote_id, item_id, qty, unit_price_cents, standard_cost_cents, configuration)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                quote_id,
                line.item_id,
                line.qty,
                unit_price_cents,
                standard_cost_cents,
                configuration
            ],
        )?;
    }
    tx.commit()?;
    Ok(quote_id)
}

pub fn get_quote(conn: &Connection, quote_id: i64) -> Result<Option<QuoteView>> {
    match fetch_quote(conn, quote_id) {
        Ok(q) => Ok(Some(q)),
        Err(LedgerError::Invalid(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn list_quotes(conn: &Connection) -> Result<Vec<QuoteView>> {
    let mut stmt = conn.prepare("SELECT id FROM quotes ORDER BY id")?;
    let ids = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for id in ids {
        out.push(fetch_quote(conn, id?)?);
    }
    Ok(out)
}

/// Quote state machine: draft → sent → accepted | declined | expired.
pub fn send_quote(conn: &mut Connection, quote_id: i64) -> Result<()> {
    transition(
        conn,
        "quotes",
        "quote",
        quote_id,
        Transition {
            from: &["draft"],
            to: "sent",
            action: "send",
            hint: "only a draft quote can be sent",
        },
    )
}

pub fn accept_quote(conn: &mut Connection, quote_id: i64) -> Result<()> {
    // Configurator gate: a configured line may ride a quote into
    // acceptance only when its configuration evaluates Consistent
    // under the current model. Incomplete is a drafting state, never
    // an accepted one. The line's snapshotted configuration and prices
    // are *not* rewritten by this check — it only gates the verdict.
    let quote = fetch_quote(conn, quote_id)?;
    if quote.status == "sent" {
        for line in &quote.lines {
            let Some(snapshot) = &line.configuration else {
                continue;
            };
            let model = config_model_for_item(conn, line.item_id)?.ok_or_else(|| {
                LedgerError::Invalid(format!(
                    "item {} no longer has a variant configuration model",
                    line.item_id
                ))
            })?;
            let values: BTreeMap<String, ConfigValue> = snapshot
                .get("values")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .ok_or_else(|| {
                    LedgerError::Invalid(format!(
                        "the stored configuration for {} is unreadable",
                        line.sku
                    ))
                })?;
            let evaluation = evaluate(&model, &values);
            match evaluation.verdict {
                Verdict::Consistent => {}
                Verdict::Incomplete { missing } => {
                    return Err(LedgerError::Invalid(format!(
                        "cannot accept quote {quote_id}: the configuration for {} is incomplete — missing {}",
                        line.sku,
                        missing.join(", ")
                    )));
                }
                Verdict::Inconsistent => {
                    return Err(LedgerError::Configuration {
                        message: format!(
                            "cannot accept quote {quote_id}: the configuration for {} is inconsistent",
                            line.sku
                        ),
                        explanations: evaluation.explanations,
                    });
                }
            }
        }
    }
    transition(
        conn,
        "quotes",
        "quote",
        quote_id,
        Transition {
            from: &["sent"],
            to: "accepted",
            action: "accept",
            hint: "a quote must be sent before it can be accepted",
        },
    )
}

pub fn decline_quote(conn: &mut Connection, quote_id: i64) -> Result<()> {
    transition(
        conn,
        "quotes",
        "quote",
        quote_id,
        Transition {
            from: &["sent"],
            to: "declined",
            action: "decline",
            hint: "only a sent quote can be declined",
        },
    )
}

pub fn expire_quote(conn: &mut Connection, quote_id: i64) -> Result<()> {
    transition(
        conn,
        "quotes",
        "quote",
        quote_id,
        Transition {
            from: &["sent"],
            to: "expired",
            action: "expire",
            hint: "only a sent quote can expire",
        },
    )
}

// ---------------------------------------------------------------------------
// Day 2: order-to-cash — sales orders
// ---------------------------------------------------------------------------

/// Create a sales order from an **accepted** quote, copying its customer
/// and lines. Any other quote status is refused. One order per quote.
pub fn create_order_from_quote(conn: &mut Connection, quote_id: i64) -> Result<i64> {
    let tx = conn.transaction()?;
    let (customer_id, status): (i64, String) = tx
        .query_row(
            "SELECT customer_id, status FROM quotes WHERE id = ?1",
            params![quote_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(missing("quote", quote_id))?;
    if status != "accepted" {
        return Err(LedgerError::Invalid(format!(
            "cannot create an order from quote {quote_id}: it is '{status}' — \
             orders are created only from accepted quotes"
        )));
    }
    let existing: i64 = tx.query_row(
        "SELECT COUNT(*) FROM sales_orders WHERE quote_id = ?1",
        params![quote_id],
        |r| r.get(0),
    )?;
    if existing > 0 {
        return Err(LedgerError::Invalid(format!(
            "quote {quote_id} already has a sales order"
        )));
    }
    tx.execute(
        "INSERT INTO sales_orders (quote_id, customer_id, status)
         VALUES (?1, ?2, 'open')",
        params![quote_id, customer_id],
    )?;
    let order_id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO order_lines (order_id, item_id, qty, unit_price_cents, standard_cost_cents, configuration)
         SELECT ?1, item_id, qty, unit_price_cents, standard_cost_cents, configuration
         FROM quote_lines WHERE quote_id = ?2",
        params![order_id, quote_id],
    )?;
    tx.commit()?;
    Ok(order_id)
}

pub fn get_order(conn: &Connection, order_id: i64) -> Result<Option<OrderView>> {
    match fetch_order(conn, order_id) {
        Ok(o) => Ok(Some(o)),
        Err(LedgerError::Invalid(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn list_orders(conn: &Connection) -> Result<Vec<OrderView>> {
    let mut stmt = conn.prepare("SELECT id FROM sales_orders ORDER BY id")?;
    let ids = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for id in ids {
        out.push(fetch_order(conn, id?)?);
    }
    Ok(out)
}

/// Order state machine: open → fulfilled | cancelled.
pub fn fulfil_order(conn: &mut Connection, order_id: i64) -> Result<()> {
    transition(
        conn,
        "sales_orders",
        "sales order",
        order_id,
        Transition {
            from: &["open"],
            to: "fulfilled",
            action: "fulfil",
            hint: "only an open order can be fulfilled",
        },
    )
}

pub fn cancel_order(conn: &mut Connection, order_id: i64) -> Result<()> {
    transition(
        conn,
        "sales_orders",
        "sales order",
        order_id,
        Transition {
            from: &["open"],
            to: "cancelled",
            action: "cancel",
            hint: "only an open order can be cancelled",
        },
    )
}

// ---------------------------------------------------------------------------
// Day 2: order-to-cash — invoices and payments
// ---------------------------------------------------------------------------

/// Create an invoice in `draft` from a sales order, copying its customer
/// and lines. A cancelled order cannot be invoiced. One invoice per order.
pub fn create_invoice_from_order(conn: &mut Connection, order_id: i64) -> Result<i64> {
    let tx = conn.transaction()?;
    let (customer_id, status): (i64, String) = tx
        .query_row(
            "SELECT customer_id, status FROM sales_orders WHERE id = ?1",
            params![order_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(missing("sales order", order_id))?;
    if status == "cancelled" {
        return Err(LedgerError::Invalid(format!(
            "cannot create an invoice from sales order {order_id}: it is cancelled"
        )));
    }
    let existing: i64 = tx.query_row(
        "SELECT COUNT(*) FROM invoices WHERE order_id = ?1",
        params![order_id],
        |r| r.get(0),
    )?;
    if existing > 0 {
        return Err(LedgerError::Invalid(format!(
            "sales order {order_id} already has an invoice"
        )));
    }
    tx.execute(
        "INSERT INTO invoices (order_id, customer_id, status)
         VALUES (?1, ?2, 'draft')",
        params![order_id, customer_id],
    )?;
    let invoice_id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO invoice_lines (invoice_id, item_id, qty, unit_price_cents, standard_cost_cents, configuration)
         SELECT ?1, item_id, qty, unit_price_cents, standard_cost_cents, configuration
         FROM order_lines WHERE order_id = ?2",
        params![invoice_id, order_id],
    )?;
    tx.commit()?;
    Ok(invoice_id)
}

pub fn get_invoice(conn: &Connection, invoice_id: i64) -> Result<Option<InvoiceView>> {
    match fetch_invoice(conn, invoice_id) {
        Ok(i) => Ok(Some(i)),
        Err(LedgerError::Invalid(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn list_invoices(conn: &Connection) -> Result<Vec<InvoiceView>> {
    let mut stmt = conn.prepare("SELECT id FROM invoices ORDER BY id")?;
    let ids = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for id in ids {
        out.push(fetch_invoice(conn, id?)?);
    }
    Ok(out)
}

/// Invoice state machine: draft → issued → partially_paid → paid | void.
/// (`paid` is reached only through payments; `void` from any unpaid state.)
pub fn issue_invoice(conn: &mut Connection, invoice_id: i64) -> Result<()> {
    transition(
        conn,
        "invoices",
        "invoice",
        invoice_id,
        Transition {
            from: &["draft"],
            to: "issued",
            action: "issue",
            hint: "only a draft invoice can be issued",
        },
    )
}

pub fn void_invoice(conn: &mut Connection, invoice_id: i64) -> Result<()> {
    transition(
        conn,
        "invoices",
        "invoice",
        invoice_id,
        Transition {
            from: &["draft", "issued", "partially_paid"],
            to: "void",
            action: "void",
            hint: "a paid invoice cannot be voided — corrections are reversing entries",
        },
    )
}

/// Record a payment against an issued invoice and advance its status
/// (`issued` → `partially_paid` → `paid`). Payments must be positive and
/// cumulative payments may not exceed the invoice total — overpayment is
/// refused, never silently accepted.
pub fn record_payment(conn: &mut Connection, invoice_id: i64, amount_cents: Cents) -> Result<i64> {
    if amount_cents <= 0 {
        return Err(LedgerError::Invalid(
            "a payment amount must be positive".to_string(),
        ));
    }
    let tx = conn.transaction()?;
    let status: String = tx
        .query_row(
            "SELECT status FROM invoices WHERE id = ?1",
            params![invoice_id],
            |r| r.get(0),
        )
        .map_err(missing("invoice", invoice_id))?;
    if status != "issued" && status != "partially_paid" {
        return Err(LedgerError::Invalid(format!(
            "cannot record a payment on invoice {invoice_id}: it is '{status}' — \
             payments are accepted only once an invoice is issued"
        )));
    }
    let total_cents: Cents = tx.query_row(
        "SELECT COALESCE(SUM(qty * unit_price_cents), 0)
         FROM invoice_lines WHERE invoice_id = ?1",
        params![invoice_id],
        |r| r.get(0),
    )?;
    let paid_cents: Cents = tx.query_row(
        "SELECT COALESCE(SUM(amount_cents), 0) FROM payments WHERE invoice_id = ?1",
        params![invoice_id],
        |r| r.get(0),
    )?;
    if paid_cents + amount_cents > total_cents {
        return Err(LedgerError::Invalid(format!(
            "payment of {amount_cents} cents exceeds the balance of invoice {invoice_id}: \
             it totals {total_cents} cents with {paid_cents} cents already paid"
        )));
    }
    tx.execute(
        "INSERT INTO payments (invoice_id, amount_cents, status)
         VALUES (?1, ?2, 'recorded')",
        params![invoice_id, amount_cents],
    )?;
    let payment_id = tx.last_insert_rowid();
    let new_status = if paid_cents + amount_cents == total_cents {
        "paid"
    } else {
        "partially_paid"
    };
    tx.execute(
        "UPDATE invoices SET status = ?1 WHERE id = ?2",
        params![new_status, invoice_id],
    )?;
    tx.commit()?;
    Ok(payment_id)
}

pub fn list_payments(conn: &Connection) -> Result<Vec<PaymentView>> {
    let mut stmt =
        conn.prepare("SELECT id, invoice_id, amount_cents, status FROM payments ORDER BY id")?;
    let rows = stmt.query_map([], |row| {
        Ok(PaymentView {
            id: row.get(0)?,
            invoice_id: row.get(1)?,
            amount_cents: row.get(2)?,
            status: row.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Payments recorded against one invoice, oldest first.
pub fn list_payments_for_invoice(conn: &Connection, invoice_id: i64) -> Result<Vec<PaymentView>> {
    let mut stmt = conn.prepare(
        "SELECT id, invoice_id, amount_cents, status FROM payments
         WHERE invoice_id = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map(params![invoice_id], |row| {
        Ok(PaymentView {
            id: row.get(0)?,
            invoice_id: row.get(1)?,
            amount_cents: row.get(2)?,
            status: row.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Day 2: order-to-cash — counts (health checks, tests)
// ---------------------------------------------------------------------------

/// Count of seeded customers.
pub fn customer_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM customers", [], |r| r.get(0))?)
}

/// Count of seeded items.
pub fn item_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))?)
}

/// Count of quotes (any status).
pub fn quote_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM quotes", [], |r| r.get(0))?)
}

/// Count of sales orders (any status).
pub fn order_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM sales_orders", [], |r| r.get(0))?)
}

/// Count of invoices (any status).
pub fn invoice_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM invoices", [], |r| r.get(0))?)
}

/// Count of recorded payments.
pub fn payment_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM payments", [], |r| r.get(0))?)
}
