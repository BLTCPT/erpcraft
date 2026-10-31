# ERPCraft Day 2 prep — order-to-cash entities (no GL yet)

Prepared Mon Oct 26 evening from the Day-1 code as built. Day 2 per the sprint
plan (`~/workspace/your_files/erpcraft-hackathon-sprint-plan.md`):
entities + state machine + transitions; exit criterion is
quote → order → invoice → payment flowing in tests, with **no GL postings**
(that is Day 3, Wed Oct 28, against the $129,975.00 oracle).

## What Day 1 gives Day 2 to build on

- `erpcraft-core/src/lib.rs`: `accounts`, `journal_entries`, `journal_lines`
  only. Money is `Cents` (i64). Invariants are DB triggers; journal is
  append-only. Frozen demo data: Cascade Precision Parts; customers
  Acme / Beta / Delta / Epsilon; standard cost $68.00/unit
  (`STANDARD_COST_CENTS = 6_800`); opening trial balance $120,500.00/side.
- `erpcraft-server/src/main.rs`: thin Axum shell, routes
  `/`, `/health`, `/accounts`, `/trial-balance`, `/journal-entries` (GET/POST).
  New Day-2 routes should follow the same pattern: handler → core fn → JSON,
  invariant violations as 400 with the core error string.

## Proposed schema additions (in `erpcraft-core`, same SCHEMA batch)

- `customers (id, name UNIQUE, created_at)` — seed the four frozen names.
- `items (id, sku UNIQUE, name, unit_price_cents, standard_cost_cents)` —
  seed one finished bracket at standard cost 6_800; price is a Day-2
  decision, keep it a named constant next to `STANDARD_COST_CENTS`, never f64.
- `quotes (id, customer_id FK, status, created_at)` +
  `quote_lines (id, quote_id FK, item_id FK, qty, unit_price_cents)`.
- `sales_orders`, `invoices`, `payments` with the same header/line shape;
  each header carries `source_doc` linkage to its parent
  (order → quote_id, invoice → order_id, payment → invoice_id) so the Day-6
  audit trail can trace any document to its source in one view.
- Document totals are derived from lines (qty × unit_price_cents, integer
  cents), never stored as floats and never accepted from the client.

## State machines (enforce in core, CHECK constraints on status)

- Quote: `draft → sent → accepted | declined | expired`
- Sales order: `open → fulfilled | cancelled` (created only from an
  `accepted` quote)
- Invoice: `draft → issued → partially_paid → paid | void`
  (`void` is a status on the document, never a delete; the GL correcting
  entry is Day 3/6 work)
- Payment: `recorded` only, amount > 0, cumulative payments against an
  invoice may not exceed its total (overpayment is a Day-7 edge case —
  reject for now, do not silently accept)

Illegal transitions return `LedgerError::Invalid` with a plain-language
message, matching Day 1's error style.

## Tests that satisfy the Day-2 exit criterion

1. Full flow: quote (Acme, qty 40) → accept → order → invoice → payment in
   full; invoice status ends `paid`; every total equals sum of lines.
2. Partial payment: invoice → `partially_paid`, second payment → `paid`.
3. Illegal jumps rejected: invoice from a non-accepted quote, payment on a
   draft invoice, payment exceeding the invoice total, order from a
   declined quote.
4. No-GL guard: `entry_count` / trial balance unchanged by the whole flow —
   this is the test that keeps Day 2 and Day 3 cleanly separated.
5. Seed determinism: customers/items seeded exactly once on a fresh DB
   (`seed_if_empty` pattern), second open is a no-op.

## Explicitly NOT Day 2

- GL postings for any transition (Day 3), MCP/CLI surface (Day 4),
  NL agent (Day 5, Meta Model API / Baseten Muse Spark Open),
  inventory decrement / COGS recognition (park unless Day 3 oracle needs it).
