//! Day 6: the audit trail — every posted journal entry, fully
//! explainable: who posted it, what document it belongs to, when, and
//! why, with the provenance chain (document → entries → reversals)
//! walkable in both directions.
//!
//! Nothing here writes. The audit surface is a read model over the
//! append-only journal plus the document headers that already record
//! their entry ids (the Day-3 double-post guards double as the audit
//! links — no new bookkeeping was added to the posting path):
//!
//! * an invoice's `gl_entry_id` / `cogs_entry_id` (issuance),
//! * a payment's `gl_entry_id`,
//! * a sales return's two reversing entries in `sales_return_lines`,
//!   each carrying `reverses_entry_id` back to the entry it corrects.
//!
//! Agent-posted entries carry `source: "agent"` and the verbatim
//! operator prompt inside the memo (the Day-5 audit decision); the
//! audit record extracts that prompt into its own field so a reviewer
//! sees exactly what the human was asked to confirm.

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::{get_entry, list_entries, JournalEntryView, LedgerError, Result};

/// A business document an entry belongs to (or a chain node).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditDocRef {
    /// "quote" | "sales_order" | "invoice" | "payment" | "sales_return"
    pub kind: String,
    pub id: i64,
    /// Plain-language label, e.g. "invoice #3 (Acme)".
    pub label: String,
}

/// One step of a document's provenance chain: the document and the
/// journal entries it posted (empty for documents that post nothing,
/// like quotes and orders — they are in the chain because provenance
/// starts at the quote, not at the first posting).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditChainNode {
    pub kind: String,
    pub id: i64,
    pub label: String,
    pub entry_ids: Vec<i64>,
}

/// One journal entry, fully explained.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    pub entry: JournalEntryView,
    /// Who originated the entry, in plain language: "system seed",
    /// "posting engine", "operator — CLI", "MCP client",
    /// "NL agent (human-confirmed)", "manual journal", or the raw
    /// source tag when it is none of those.
    pub who: String,
    /// The role this entry plays for its document: "revenue",
    /// "cogs", "payment", "return_revenue", "return_cogs",
    /// "opening", "manual" — derived from the link, never stored.
    pub role: String,
    /// One plain-language sentence: what this entry is and why it
    /// posted.
    pub summary: String,
    /// The document this entry belongs to, when it has one.
    pub document: Option<AuditDocRef>,
    /// The verbatim operator prompt, for agent-posted entries
    /// (extracted from the memo; `None` for every other source).
    pub prompt: Option<String>,
    /// The full provenance chain of the entry's document, quote →
    /// order → invoice → payments → returns, with each document's
    /// entry ids. Empty for entries with no document.
    pub chain: Vec<AuditChainNode>,
    /// Entries that reverse this one (corrections pointing back).
    pub reversed_by: Vec<i64>,
}

/// Filters for [`audit_trail`]. All optional; dates are inclusive
/// `YYYY-MM-DD` bounds on the entry date.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditFilter {
    /// Exact source tag match ("agent", "cli", "mcp", "invoice:3", …).
    pub source: Option<String>,
    /// Document filter: "invoice:3", "payment:2", "sales_return:3"
    /// (a return's id is its invoice's id — returns have no header
    /// row of their own), "sales_order:1", "quote:1".
    pub document: Option<String>,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
}

/// Every posted entry matching the filter, oldest first, each fully
/// explained. Drafts are not audit records — nothing has happened yet.
pub fn audit_trail(conn: &Connection, filter: &AuditFilter) -> Result<Vec<AuditRecord>> {
    let doc_filter = match &filter.document {
        Some(d) => Some(parse_document_filter(d)?),
        None => None,
    };
    let mut out = Vec::new();
    for entry in list_entries(conn)? {
        if entry.status != "posted" {
            continue;
        }
        if let Some(source) = &filter.source {
            if &entry.source != source {
                continue;
            }
        }
        if let Some(from) = &filter.from_date {
            if entry.entry_date < *from {
                continue;
            }
        }
        if let Some(to) = &filter.to_date {
            if entry.entry_date > *to {
                continue;
            }
        }
        let record = explain(conn, entry)?;
        if let Some((kind, id)) = &doc_filter {
            let in_document = record
                .document
                .as_ref()
                .is_some_and(|d| &d.kind == kind && &d.id == id)
                || record.chain.iter().any(|n| &n.kind == kind && &n.id == id);
            if !in_document {
                continue;
            }
        }
        out.push(record);
    }
    Ok(out)
}

/// One entry, fully explained. `None` when the entry does not exist.
pub fn audit_entry(conn: &Connection, entry_id: i64) -> Result<Option<AuditRecord>> {
    match get_entry(conn, entry_id)? {
        Some(entry) => Ok(Some(explain(conn, entry)?)),
        None => Ok(None),
    }
}

fn parse_document_filter(raw: &str) -> Result<(String, i64)> {
    let (kind, id) = raw.split_once(':').ok_or_else(|| {
        LedgerError::Invalid(format!(
            "document filter '{raw}' must look like 'invoice:3'"
        ))
    })?;
    let id: i64 = id.parse().map_err(|_| {
        LedgerError::Invalid(format!("document filter '{raw}' has a non-numeric id"))
    })?;
    Ok((kind.to_string(), id))
}

/// Extract the verbatim prompt from an agent memo. The Day-5 adapter
/// writes `{summary} — prompt: "{verbatim}"`; anything else yields
/// `None` (a hand-written memo is not a prompt).
pub fn agent_prompt(memo: &str) -> Option<String> {
    let marker = " — prompt: \"";
    let start = memo.find(marker)? + marker.len();
    let rest = &memo[start..];
    let end = rest.rfind('"')?;
    Some(rest[..end].to_string())
}

fn who_for(source: &str) -> String {
    match source {
        "opening" => "system seed".to_string(),
        "cli" => "operator — CLI".to_string(),
        "mcp" => "MCP client".to_string(),
        "agent" => "NL agent (human-confirmed)".to_string(),
        "manual" => "manual journal".to_string(),
        other if other.starts_with("invoice:") => "posting engine".to_string(),
        other if other.starts_with("payment:") => "posting engine".to_string(),
        other if other.starts_with("return:") => "posting engine".to_string(),
        other => other.to_string(),
    }
}

// --- document links ---------------------------------------------------------

struct InvoiceLink {
    invoice_id: i64,
    customer_name: String,
    role: &'static str,
}

fn invoice_link_for_entry(conn: &Connection, entry_id: i64) -> Result<Option<InvoiceLink>> {
    let mut stmt = conn.prepare(
        "SELECT i.id, c.name,
                CASE WHEN i.gl_entry_id = ?1 THEN 'revenue' ELSE 'cogs' END
         FROM invoices i
         JOIN customers c ON c.id = i.customer_id
         WHERE i.gl_entry_id = ?1 OR i.cogs_entry_id = ?1",
    )?;
    let mut rows = stmt.query(params![entry_id])?;
    match rows.next()? {
        Some(row) => Ok(Some(InvoiceLink {
            invoice_id: row.get(0)?,
            customer_name: row.get(1)?,
            role: if row.get::<_, String>(2)? == "revenue" {
                "revenue"
            } else {
                "cogs"
            },
        })),
        None => Ok(None),
    }
}

struct PaymentLink {
    payment_id: i64,
    invoice_id: i64,
    amount_cents: i64,
}

fn payment_link_for_entry(conn: &Connection, entry_id: i64) -> Result<Option<PaymentLink>> {
    let mut stmt =
        conn.prepare("SELECT id, invoice_id, amount_cents FROM payments WHERE gl_entry_id = ?1")?;
    let mut rows = stmt.query(params![entry_id])?;
    match rows.next()? {
        Some(row) => Ok(Some(PaymentLink {
            payment_id: row.get(0)?,
            invoice_id: row.get(1)?,
            amount_cents: row.get(2)?,
        })),
        None => Ok(None),
    }
}

struct ReturnLink {
    invoice_id: i64,
    role: &'static str,
}

fn return_link_for_entry(conn: &Connection, entry_id: i64) -> Result<Option<ReturnLink>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT invoice_id,
                CASE WHEN revenue_entry_id = ?1 THEN 'return_revenue' ELSE 'return_cogs' END
         FROM sales_return_lines
         WHERE revenue_entry_id = ?1 OR cogs_entry_id = ?1",
    )?;
    let mut rows = stmt.query(params![entry_id])?;
    match rows.next()? {
        Some(row) => Ok(Some(ReturnLink {
            invoice_id: row.get(0)?,
            role: if row.get::<_, String>(1)? == "return_revenue" {
                "return_revenue"
            } else {
                "return_cogs"
            },
        })),
        None => Ok(None),
    }
}

// --- the chain ---------------------------------------------------------------

/// The full provenance chain of one invoice's lifecycle: quote →
/// sales order → invoice (issuance entries) → payments → returns.
/// Returns the nodes plus the invoice's customer label parts.
fn invoice_chain(conn: &Connection, invoice_id: i64) -> Result<Vec<AuditChainNode>> {
    let (order_id, quote_id, customer_name, gl_entry_id, cogs_entry_id): (
        i64,
        i64,
        String,
        Option<i64>,
        Option<i64>,
    ) = conn.query_row(
        "SELECT i.order_id, o.quote_id, c.name, i.gl_entry_id, i.cogs_entry_id
         FROM invoices i
         JOIN sales_orders o ON o.id = i.order_id
         JOIN customers c ON c.id = i.customer_id
         WHERE i.id = ?1",
        params![invoice_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let mut nodes = vec![
        AuditChainNode {
            kind: "quote".to_string(),
            id: quote_id,
            label: format!("quote #{quote_id} ({customer_name})"),
            entry_ids: Vec::new(),
        },
        AuditChainNode {
            kind: "sales_order".to_string(),
            id: order_id,
            label: format!("sales order #{order_id} ({customer_name})"),
            entry_ids: Vec::new(),
        },
        AuditChainNode {
            kind: "invoice".to_string(),
            id: invoice_id,
            label: format!("invoice #{invoice_id} ({customer_name})"),
            entry_ids: [gl_entry_id, cogs_entry_id].into_iter().flatten().collect(),
        },
    ];
    {
        let mut stmt =
            conn.prepare("SELECT id, gl_entry_id FROM payments WHERE invoice_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map(params![invoice_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?))
        })?;
        for row in rows {
            let (payment_id, entry_id) = row?;
            nodes.push(AuditChainNode {
                kind: "payment".to_string(),
                id: payment_id,
                label: format!("payment #{payment_id} (invoice #{invoice_id})"),
                entry_ids: entry_id.into_iter().collect(),
            });
        }
    }
    {
        // A return posts one pair of entries per call; the lines of
        // one call share both entry ids, so DISTINCT pairs are the
        // individual returns.
        let mut stmt = conn.prepare(
            "SELECT DISTINCT revenue_entry_id, cogs_entry_id
             FROM sales_return_lines WHERE invoice_id = ?1 ORDER BY revenue_entry_id",
        )?;
        let rows = stmt.query_map(params![invoice_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (revenue_id, cogs_id) = row?;
            nodes.push(AuditChainNode {
                kind: "sales_return".to_string(),
                id: invoice_id,
                label: format!("sales return (invoice #{invoice_id})"),
                entry_ids: vec![revenue_id, cogs_id],
            });
        }
    }
    Ok(nodes)
}

fn invoice_customer(conn: &Connection, invoice_id: i64) -> Result<String> {
    Ok(conn.query_row(
        "SELECT c.name FROM invoices i JOIN customers c ON c.id = i.customer_id WHERE i.id = ?1",
        params![invoice_id],
        |r| r.get(0),
    )?)
}

fn fmt_money(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.abs();
    format!("{sign}${}.{:02}", abs / 100, abs % 100)
}

fn explain(conn: &Connection, entry: JournalEntryView) -> Result<AuditRecord> {
    let who = who_for(&entry.source);
    let prompt = if entry.source == "agent" {
        agent_prompt(&entry.memo)
    } else {
        None
    };
    let mut reversed_by = Vec::new();
    {
        let mut stmt = conn
            .prepare("SELECT id FROM journal_entries WHERE reverses_entry_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map(params![entry.id], |r| r.get::<_, i64>(0))?;
        for r in rows {
            reversed_by.push(r?);
        }
    }

    // Document link, most specific first: a return's entries also
    // parse as "return:<invoice>", a payment's as "payment:<id>" —
    // the header/line tables are the ground truth.
    if let Some(link) = return_link_for_entry(conn, entry.id)? {
        let customer = invoice_customer(conn, link.invoice_id)?;
        let (role, summary) = if link.role == "return_revenue" {
            (
                "return_revenue",
                format!(
                    "Sales return revenue reversal for invoice #{} ({customer}) — reverses entry #{}.",
                    link.invoice_id,
                    entry.reverses_entry_id.map(|i| i.to_string()).unwrap_or_else(|| "?".to_string())
                ),
            )
        } else {
            (
                "return_cogs",
                format!(
                    "Sales return restock for invoice #{} ({customer}) — returned units back into finished goods, reversing entry #{}.",
                    link.invoice_id,
                    entry.reverses_entry_id.map(|i| i.to_string()).unwrap_or_else(|| "?".to_string())
                ),
            )
        };
        return Ok(AuditRecord {
            document: Some(AuditDocRef {
                kind: "sales_return".to_string(),
                id: link.invoice_id,
                label: format!("sales return (invoice #{})", link.invoice_id),
            }),
            chain: invoice_chain(conn, link.invoice_id)?,
            role: role.to_string(),
            summary,
            who,
            prompt,
            reversed_by,
            entry,
        });
    }
    if let Some(link) = payment_link_for_entry(conn, entry.id)? {
        let customer = invoice_customer(conn, link.invoice_id)?;
        return Ok(AuditRecord {
            document: Some(AuditDocRef {
                kind: "payment".to_string(),
                id: link.payment_id,
                label: format!("payment #{} (invoice #{})", link.payment_id, link.invoice_id),
            }),
            chain: invoice_chain(conn, link.invoice_id)?,
            role: "payment".to_string(),
            summary: format!(
                "Payment of {} against invoice #{} ({customer}) — posted when the payment was recorded.",
                fmt_money(link.amount_cents),
                link.invoice_id
            ),
            who,
            prompt,
            reversed_by,
            entry,
        });
    }
    if let Some(link) = invoice_link_for_entry(conn, entry.id)? {
        let (role, summary) = if link.role == "revenue" {
            (
                "revenue",
                format!(
                    "Sales revenue for invoice #{} ({}) — posted by the posting engine when the invoice was issued.",
                    link.invoice_id, link.customer_name
                ),
            )
        } else {
            (
                "cogs",
                format!(
                    "Cost of goods sold for invoice #{} ({}) — posted at issuance from the invoice lines' snapshotted costs.",
                    link.invoice_id, link.customer_name
                ),
            )
        };
        return Ok(AuditRecord {
            document: Some(AuditDocRef {
                kind: "invoice".to_string(),
                id: link.invoice_id,
                label: format!("invoice #{} ({})", link.invoice_id, link.customer_name),
            }),
            chain: invoice_chain(conn, link.invoice_id)?,
            role: role.to_string(),
            summary,
            who,
            prompt,
            reversed_by,
            entry,
        });
    }

    // No document: opening, manual/door/agent journal entries.
    let (role, summary) = match entry.source.as_str() {
        "opening" => (
            "opening".to_string(),
            "Opening balances — posted by the seed when this ledger was created.".to_string(),
        ),
        "agent" => (
            "manual".to_string(),
            match &prompt {
                Some(p) => format!(
                    "Posted by the NL agent after a human confirmed the proposal. Prompt: \"{p}\""
                ),
                None => "Posted by the NL agent after a human confirmed the proposal.".to_string(),
            },
        ),
        "cli" => (
            "manual".to_string(),
            "Manual journal entry posted by an operator through the CLI.".to_string(),
        ),
        "mcp" => (
            "manual".to_string(),
            "Manual journal entry posted through the MCP server.".to_string(),
        ),
        _ => (
            "manual".to_string(),
            format!("Manual journal entry (source: {}).", entry.source),
        ),
    };
    Ok(AuditRecord {
        document: None,
        chain: Vec::new(),
        role,
        summary,
        who,
        prompt,
        reversed_by,
        entry,
    })
}
