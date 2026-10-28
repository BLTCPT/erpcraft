# ERPCraft Day 3 prep — GL posting engine + oracle suite

Prepared Tue Oct 27 from the Day-1 code as built, the Day-2 prep spec
(`DAY2_PREP.md`), and the oracle day spec
(`~/workspace/your_files/erpcraft-oracle-day-spec.md`). Day 3 per the sprint
plan (`~/workspace/your_files/erpcraft-hackathon-sprint-plan.md`):
each order-to-cash transition emits balanced journal entries; trial balance
+ account ledger views; exit criterion is the oracle suite — the scripted
business day ending on the hand-computed trial balance of **$129,975.00
per side**.

House rules (`AGENTS.md`) hold throughout: money is integer cents, the DB
triggers are the enforcer, the journal is append-only, corrections are
reversing entries via `reverses_entry_id`, tests accompany every command.

## Reconciliation addendum (Wed Oct 28, after the configurator landed)

DAY3_PREP was written Oct 27 from the Day-1 code, before Day 2 shipped
and before the variant configurator landed that night (commits 16d470b /
a60a01b / f709c6f). Checked against the code as built Oct 28 — the
posting map and oracle assertions stand, with these corrections:

- **COGS must read the invoice-line snapshot, not the item master.**
  `invoice_lines.standard_cost_cents` is snapshotted at quote time and
  copied unchanged quote → order → invoice. For configured lines that
  snapshot is the rolled-up cost (base cost + Σ component cost deltas ×
  component qty), not the item's $68.00. Posting COGS as
  Σ(qty × items.standard_cost_cents) would still pass the oracle (its
  lines are unconfigured, snapshot = master) but would silently
  mis-post every configured invoice. Rule: COGS = Σ(qty ×
  invoice_lines.standard_cost_cents); revenue likewise comes from the
  lines' snapshotted unit prices (the invoice total already derives
  from lines, so this is automatic if the total is the entry amount).
- **Line standard cost is nullable** (`Option<Cents>` on the line
  structs; legacy rows may carry NULL). A NULL line cost at issuance is
  the missing-standard-cost case in test 8: refuse the transition,
  post nothing, invoice stays `draft`.
- **`gl_entry_id` / `cogs_entry_id` columns do not exist yet** on
  invoices/payments. Add them with the existing `ensure_column`
  migration pattern (the same one that added the line snapshot
  columns) rather than a schema rewrite.
- `journal_entries.source` and `reverses_entry_id` already exist as
  the prep assumed — no change needed there. `issue_invoice` is
  currently a bare `transition()` call; the revenue + COGS postings
  hook inside that same transaction.

## Dependency on Day 2 (read this first)

This spec assumes Day 2 landed as prepped: `customers`, `items`, `quotes`,
`sales_orders`, `invoices`, `payments` tables with the state machines in
`DAY2_PREP.md`, and the Day-2 no-GL guard test green. If Day 2's actual
table/field names differ, adapt the names below to the code as built —
the posting map and the oracle assertions do not change. Day 3's first
job is deleting/inverting the no-GL guard: the full flow now MUST move
the trial balance, in exactly the amounts below.

## What Day 1 already gives Day 3

- `journal_entries` already carries `source TEXT DEFAULT 'manual'` and
  `reverses_entry_id` — no schema change needed for provenance. Day 3
  uses `source` values of the form `invoice:<id>`, `payment:<id>`,
  `return:<invoice_id>` so Day 6's audit trail can trace entry → document
  without a join table.
- Posting goes through the existing core post path (DB triggers reject
  unbalanced / <2-line / non-positive entries). The posting engine is a
  courtesy layer that builds correct lines; it never bypasses the
  triggers and never writes `journal_lines` directly outside that path.
- Server already exposes `/trial-balance` and `/journal-entries`.
  Day 3 adds one read view (account ledger, below).

## The posting map (the whole design)

Post at the transition, in the SAME SQLite transaction as the status
change — if the posting fails, the transition rolls back. A document
transition posts at most once: store the resulting entry id(s) on the
document header (`gl_entry_id`, `cogs_entry_id` on invoices;
`gl_entry_id` on payments) and refuse to post again if already set.
Re-driving a transition (retry, double-click, agent replay) must be a
no-op or a clean `LedgerError::Invalid`, never a duplicate entry.

| Transition | Entry | Lines |
|---|---|---|
| Invoice `draft → issued` | revenue entry, source `invoice:<id>` | Dr 1100 AR total / Cr 4000 Sales Revenue total |
| Invoice `draft → issued` | COGS entry, source `invoice:<id>` | Dr 5000 COGS Σ(qty × item standard cost) / Cr 1220 Finished Goods same |
| Payment `→ recorded` | source `payment:<id>` | Dr 1000 Cash amount / Cr 1100 AR amount |
| Sales return against an issued invoice | TWO reversing entries, each with `reverses_entry_id` set to the entry it corrects, source `return:<invoice_id>` | Dr 4010 Sales Returns (qty × price) / Cr 1100 AR; and Dr 1220 Finished Goods (qty × std cost) / Cr 5000 COGS |

Decisions baked in (flag now if any should flip):

- **Revenue + COGS both at issuance.** The oracle day's T1/T8 combine
  ship + invoice into one step, and Day 2 has no separate shipment
  entity — so issuance is the recognition point. Quote and order
  transitions post NOTHING (oracle T0 is explicitly no-posting).
- **Partial payments post their actual amount** each time; invoice
  status (`partially_paid` / `paid`) stays derived from cumulative
  payments vs total, per Day 2. No separate entry at `paid`.
- **Returns are correcting entries, never edits** — and a return is a
  Day-3 feature ONLY because oracle T11 requires it. The general
  returns UI/flow is Day 6/7 work; Day 3 needs just the core function
  `record_sales_return(invoice_id, lines)` + its entries.
- **Customer deposits do NOT post through the O2C engine on Day 3.**
  Oracle T9 (Epsilon deposit, Dr Cash / Cr 2200 Customer Deposits) is a
  manual journal entry — liability, not revenue; applying a deposit to
  a later invoice stays out of scope (Day 7 edge case at earliest).

## The rest of the oracle day is manual postings — on purpose

Oracle steps T2–T7, T9, T12–T14 (Beta's payment against opening AR, raw
material purchase on credit, supplier payment, WIP issue / labor /
completion, deposit, rent, interest, depreciation) are NOT order-to-cash
transitions. Day 3 drives them through the existing manual journal path
(`POST /journal-entries` / the core post function), exactly as a human
bookkeeper would. Building engine support for procurement/production
postings is parked scope (ROADMAP), not Day-3 work. The oracle test is
therefore a mix: T0/T1/T8/T10/T11 through the engine, the rest manual —
which is also the honest demo story.

## Account ledger view (the one new read)

`GET /accounts/<code>/entries` → the account's journal lines in entry
order with a running balance (debit-positive for asset/expense,
credit-positive for liability/equity/revenue — match however
`/trial-balance` already presents the account's side; do not invent a
second convention). Back it with a core fn returning lines + running
totals in cents. This plus the existing `/trial-balance` satisfies the
plan's "trial balance + account ledger views."

## The oracle suite (Day-3 exit criterion)

One integration test (core-level, in-memory DB) that replays the oracle
day step for step and asserts, in order:

1. Opening state: trial balance $120,500.00 per side after seed.
2. T0 quote posts nothing (entry count unchanged).
3. Each engine posting lands with the exact lines in the oracle spec
   (T1: AR/Revenue $5,000.00, COGS/FG $2,720.00 at 40 × $68; T8:
   $3,125.00 / $1,700.00 at 25 × $68; T10 payment $5,000.00).
4. T11's two entries carry `reverses_entry_id` pointing at T8's two
   entries, and T8's entries are byte-unchanged afterwards.
5. Ending trial balance equals the oracle table PER ACCOUNT, not just
   in total: Cash $53,675.00 Dr; AR $9,075.00 Dr; Raw Materials
   $9,300.00; WIP $600.00; Finished Goods $9,116.00; Equipment
   $42,000.00; Accum. Depreciation $7,150.00 Cr; AP $9,200.00 Cr;
   Customer Deposits $1,500.00 Cr; Loan $25,000.00 Cr; Equity
   $65,000.00 Cr; Retained Earnings $14,000.00 Cr; Revenue $8,125.00 Cr;
   Sales Returns $250.00 Dr; COGS $4,284.00 Dr; Rent $1,200.00;
   Interest $125.00; Depreciation $350.00 — totals $129,975.00 = $129,975.00.
6. Cross-checks from the oracle spec: net income $1,916.00; accounting
   equation net assets $116,616.00 = liabilities $35,700.00 + equity
   $79,000.00 + net income $1,916.00; zero-balance accounts stay zero
   (1120, 2100, 2400, 6030).

Plus the engine's own unit tests:

7. Double-post guard: issuing the same invoice twice yields one revenue
   entry and one COGS entry (second attempt no-op or clean error).
8. Atomicity: force a posting failure (e.g. invoice referencing an item
   with missing standard cost) → invoice status is still `draft`, no
   partial entry exists.
9. Payment on a draft invoice still rejected (Day-2 rule, now with GL
   attached); payment exceeding the invoice total still rejected, and
   the rejected payment posts nothing.
10. Return exceeding the invoiced quantity rejected; return against a
    draft invoice rejected.

## Explicitly NOT Day 3

- MCP / CLI surface (Day 4), NL agent (Day 5), audit-trail UI (Day 6 —
  Day 3 only needs the `source` / `reverses_entry_id` provenance to be
  correct), deposit application, tax (2100 stays $0), any
  procurement/production posting engine.
