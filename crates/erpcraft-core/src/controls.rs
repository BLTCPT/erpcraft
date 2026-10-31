//! Controls foundations (ENTERPRISE_GAPS.md §6 + §7): the two
//! control surfaces every SOX auditor asks about first —
//!
//! * **Fiscal periods.** A period is a named date range. Closing one
//!   freezes it: `post_entry_in` — the single posting path every door
//!   and document transition funnels through — refuses any entry
//!   dated inside a closed period. No periods defined means no
//!   gating, which is why the frozen demo day is unaffected.
//! * **Users, roles, and a payment approval rule.** A minimal user
//!   registry (name + role: admin / approver / clerk) and one
//!   segregation-of-duties rule: when `payment_approval_threshold_cents`
//!   is set in `control_settings`, a payment (AR receipt or AP
//!   payment) *above* the threshold requires an actor and a second,
//!   different approver whose role is approver or admin. The
//!   approver's name is recorded on the payment row (`approved_by`)
//!   and in the posted entry's memo — the entry's audit record.
//!   With no threshold set the rule is off and unattributed payments
//!   behave exactly as they always have.
//!
//! Deliberate limits (named in ENTERPRISE_GAPS.md): identities are
//! names, not authenticated logins — there is no password or SSO
//! here; the rule covers payments only, not journal entries or
//! purchase orders; periods gate by entry date only (no soft close,
//! no reopen workflow — a mistaken close is corrected by a follow-up
//! entry dated in an open period, never by editing history).

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::{Cents, LedgerError, Result};

/// The control_settings key for the payment approval threshold.
pub const APPROVAL_THRESHOLD_KEY: &str = "payment_approval_threshold_cents";

const ROLES: [&str; 3] = ["admin", "approver", "clerk"];

// ---------------------------------------------------------------------------
// Fiscal periods
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FiscalPeriodView {
    pub id: i64,
    pub name: String,
    /// First day of the period, `YYYY-MM-DD` (inclusive).
    pub start_date: String,
    /// Last day of the period, `YYYY-MM-DD` (inclusive).
    pub end_date: String,
    /// "open" | "closed"
    pub status: String,
}

fn fetch_period(conn: &Connection, period_id: i64) -> Result<FiscalPeriodView> {
    conn.query_row(
        "SELECT id, name, start_date, end_date, status FROM fiscal_periods WHERE id = ?1",
        params![period_id],
        |r| {
            Ok(FiscalPeriodView {
                id: r.get(0)?,
                name: r.get(1)?,
                start_date: r.get(2)?,
                end_date: r.get(3)?,
                status: r.get(4)?,
            })
        },
    )
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => {
            LedgerError::Invalid(format!("no fiscal period with id {period_id}"))
        }
        other => LedgerError::Db(other),
    })
}

pub fn list_fiscal_periods(conn: &Connection) -> Result<Vec<FiscalPeriodView>> {
    let mut stmt = conn.prepare("SELECT id FROM fiscal_periods ORDER BY start_date, id")?;
    let ids = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for id in ids {
        out.push(fetch_period(conn, id?)?);
    }
    Ok(out)
}

/// Create an open fiscal period. Dates are ISO `YYYY-MM-DD` and the
/// range is inclusive at both ends; periods may not overlap (an
/// entry date must belong to at most one period for the close gate
/// to mean anything).
pub fn create_fiscal_period(
    conn: &Connection,
    name: &str,
    start_date: &str,
    end_date: &str,
) -> Result<i64> {
    let name = name.trim();
    if name.is_empty() {
        return Err(LedgerError::Invalid(
            "a fiscal period needs a name (e.g. '2026-10')".to_string(),
        ));
    }
    for (label, date) in [("start_date", start_date), ("end_date", end_date)] {
        if !is_iso_date(date) {
            return Err(LedgerError::Invalid(format!(
                "{label} '{date}' is not an ISO date (YYYY-MM-DD)"
            )));
        }
    }
    if start_date > end_date {
        return Err(LedgerError::Invalid(format!(
            "fiscal period '{name}' starts ({start_date}) after it ends ({end_date})"
        )));
    }
    let clash: i64 = conn.query_row(
        "SELECT COUNT(*) FROM fiscal_periods
         WHERE name = ?1 OR (start_date <= ?3 AND end_date >= ?2)",
        params![name, start_date, end_date],
        |r| r.get(0),
    )?;
    if clash > 0 {
        return Err(LedgerError::Invalid(format!(
            "fiscal period '{name}' ({start_date}..{end_date}) clashes with an existing \
             period — names are unique and ranges may not overlap"
        )));
    }
    conn.execute(
        "INSERT INTO fiscal_periods (name, start_date, end_date, status)
         VALUES (?1, ?2, ?3, 'open')",
        params![name, start_date, end_date],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Close a period: from the moment it closes, every posting path
/// refuses entries dated inside it. Closing is one-way in this
/// build (see the module note on reopening).
pub fn close_fiscal_period(conn: &Connection, period_id: i64) -> Result<()> {
    let period = fetch_period(conn, period_id)?;
    if period.status == "closed" {
        return Err(LedgerError::Invalid(format!(
            "fiscal period '{}' is already closed",
            period.name
        )));
    }
    conn.execute(
        "UPDATE fiscal_periods SET status = 'closed', closed_at = datetime('now')
         WHERE id = ?1",
        params![period_id],
    )?;
    Ok(())
}

/// The closed period containing `entry_date`, if any. Called by
/// `post_entry_in` on every posting — with no periods defined this
/// is a single indexed lookup that finds nothing.
pub(crate) fn closed_period_for(conn: &Connection, entry_date: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM fiscal_periods
         WHERE status = 'closed' AND ?1 >= start_date AND ?1 <= end_date
         LIMIT 1",
    )?;
    let mut rows = stmt.query_map(params![entry_date], |r| r.get::<_, String>(0))?;
    match rows.next() {
        Some(Ok(name)) => Ok(Some(name)),
        Some(Err(e)) => Err(LedgerError::Db(e)),
        None => Ok(None),
    }
}

/// Shape and range check for an ISO date: `YYYY-MM-DD` with a real
/// month (01–12) and day (01–31). Per-month day counts (Feb 30) are
/// not policed — the gate compares date strings, and a nonsense day
/// simply matches no entries.
fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    if !s
        .chars()
        .enumerate()
        .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
    {
        return false;
    }
    let month: u32 = s[5..7].parse().unwrap_or(0);
    let day: u32 = s[8..10].parse().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

// ---------------------------------------------------------------------------
// Users and roles
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserView {
    pub id: i64,
    pub name: String,
    /// "admin" | "approver" | "clerk"
    pub role: String,
}

pub fn create_user(conn: &Connection, name: &str, role: &str) -> Result<i64> {
    let name = name.trim();
    if name.is_empty() {
        return Err(LedgerError::Invalid("a user needs a name".to_string()));
    }
    if !ROLES.contains(&role) {
        return Err(LedgerError::Invalid(format!(
            "unknown role '{role}' — roles are: {}",
            ROLES.join(", ")
        )));
    }
    let existing: i64 = conn.query_row(
        "SELECT COUNT(*) FROM users WHERE name = ?1",
        params![name],
        |r| r.get(0),
    )?;
    if existing > 0 {
        return Err(LedgerError::Invalid(format!(
            "a user named '{name}' already exists"
        )));
    }
    conn.execute(
        "INSERT INTO users (name, role) VALUES (?1, ?2)",
        params![name, role],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn list_users(conn: &Connection) -> Result<Vec<UserView>> {
    let mut stmt = conn.prepare("SELECT id, name, role FROM users ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        Ok(UserView {
            id: r.get(0)?,
            name: r.get(1)?,
            role: r.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn user_role(conn: &Connection, name: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT role FROM users WHERE name = ?1")?;
    let mut rows = stmt.query_map(params![name], |r| r.get::<_, String>(0))?;
    match rows.next() {
        Some(Ok(role)) => Ok(Some(role)),
        Some(Err(e)) => Err(LedgerError::Db(e)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Control settings + the payment approval rule
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlSettingView {
    pub key: String,
    pub value: String,
}

pub fn list_control_settings(conn: &Connection) -> Result<Vec<ControlSettingView>> {
    let mut stmt = conn.prepare("SELECT key, value FROM control_settings ORDER BY key")?;
    let rows = stmt.query_map([], |r| {
        Ok(ControlSettingView {
            key: r.get(0)?,
            value: r.get(1)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Set (or replace) the payment approval threshold, in cents. Above
/// this amount, payments need a second approver (see the module
/// docs). There is deliberately no "unset" door in this build —
/// raising the threshold is how a deployment relaxes the rule.
pub fn set_approval_threshold(conn: &Connection, threshold_cents: Cents) -> Result<()> {
    if threshold_cents < 0 {
        return Err(LedgerError::Invalid(
            "the approval threshold may not be negative".to_string(),
        ));
    }
    conn.execute(
        "INSERT INTO control_settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![APPROVAL_THRESHOLD_KEY, threshold_cents.to_string()],
    )?;
    Ok(())
}

pub fn approval_threshold(conn: &Connection) -> Result<Option<Cents>> {
    let mut stmt = conn.prepare("SELECT value FROM control_settings WHERE key = ?1")?;
    let mut rows = stmt.query_map(params![APPROVAL_THRESHOLD_KEY], |r| r.get::<_, String>(0))?;
    match rows.next() {
        Some(Ok(raw)) => Ok(raw.trim().parse::<Cents>().ok()),
        Some(Err(e)) => Err(LedgerError::Db(e)),
        None => Ok(None),
    }
}

/// The segregation-of-duties gate shared by both payment paths (AR
/// receipts in `lib.rs`, AP payments in `procurement.rs`). Returns
/// the approver's name to record when the rule fired and passed;
/// `Ok(None)` when the rule is off or the amount is at/below the
/// threshold. `what` names the document kind in refusal messages
/// ("payment" / "AP payment").
pub(crate) fn check_payment_approval(
    conn: &Connection,
    amount_cents: Cents,
    actor: Option<&str>,
    approver: Option<&str>,
    what: &str,
) -> Result<Option<String>> {
    let Some(threshold) = approval_threshold(conn)? else {
        return Ok(None);
    };
    if amount_cents <= threshold {
        return Ok(None);
    }
    let actor = actor.map(str::trim).filter(|s| !s.is_empty());
    let approver = approver.map(str::trim).filter(|s| !s.is_empty());
    let (Some(actor), Some(approver)) = (actor, approver) else {
        return Err(LedgerError::Invalid(format!(
            "this {what} of {amount_cents} cents exceeds the approval threshold of \
             {threshold} cents: it needs an actor and a second approver \
             (segregation of duties)"
        )));
    };
    if actor == approver {
        return Err(LedgerError::Invalid(format!(
            "the approver of a {what} above the threshold must be a different \
             person than the actor — '{actor}' cannot approve their own {what}"
        )));
    }
    if user_role(conn, actor)?.is_none() {
        return Err(LedgerError::Invalid(format!(
            "unknown actor '{actor}' — create the user first (create_user)"
        )));
    }
    match user_role(conn, approver)? {
        None => Err(LedgerError::Invalid(format!(
            "unknown approver '{approver}' — create the user first (create_user)"
        ))),
        Some(role) if role == "approver" || role == "admin" => Ok(Some(approver.to_string())),
        Some(role) => Err(LedgerError::Invalid(format!(
            "approver '{approver}' has role '{role}' — approving a {what} above the \
             threshold needs an approver or admin"
        ))),
    }
}
