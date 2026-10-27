# ERPCraft

**A free, open-source, clean-room ERP in Rust — AI-native from day one.**

ERPCraft takes the ArtCraft playbook — clean-room rebuilds of expensive,
closed incumbents, in pure Rust, with the source open — and applies it to
enterprise resource planning. The full vision is a complete ERP: general
ledger, order-to-cash, purchasing, inventory, manufacturing, and reporting,
owned by the people who run it, offline-capable, with no license fees and no
1990s workflows held together by million-dollar consultants.

This repository is the working core of that vision, being built in the open:
a **double-entry ledger that is provably correct**, the order-to-cash loop on
top of it, and an AI agent that drives the same commands a human does.

> ERPCraft is a clean-room implementation written from scratch. It is not
> affiliated with, endorsed by, or derived from SAP SE or any other ERP
> vendor.

## Status: early alpha — one loop, provably correct

Honest framing, in the Storytold tradition: this is an early alpha, not a
daily-driver replacement for a production ERP. What exists today is the
foundation everything else stands on, and it is built to be *verified*, not
trusted:

- **Chart of accounts** — seeded for a small manufacturer (22 accounts).
- **Double-entry journal** — the core invariant (*every posting balances,
  debits = credits*) is enforced **in the database itself** (SQLite
  triggers), not just in application code. No code path — HTTP, CLI, MCP, an
  AI agent, or raw SQL — can post an unbalanced entry.
- **Append-only ledger** — posted entries are immutable at the DB level.
  Corrections are reversing entries that reference the original. The full
  history replays from the journal.
- **Money is integer cents** — never floating point.
- **Trial balance** — computed from posted entries; both sides tie out.
- **HTTP API** — health, accounts, trial balance, journal posting/read-back
  (Axum). CLI and MCP server arrive during the sprint; the agent is a thin
  adapter over the same commands.

## The demo company

The seed data is **Cascade Precision Parts**, a fictional small Oregon
manufacturer making metal brackets (standard cost $68.00/unit). Customers in
the scripted demo day: Acme, Beta, Delta, Epsilon. Opening trial balance:
$120,500.00 on both sides. The scripted "oracle day" — 14 posting
transactions whose ending trial balance ($129,975.00) is hand-computed —
becomes the acceptance test suite for the posting engine.

## Quickstart

```sh
cargo test                    # the invariant suite — start here
cargo run -p erpcraft-server  # serves on http://127.0.0.1:3000
```

Then:

```sh
curl localhost:3000/health
curl localhost:3000/trial-balance
# Post a balanced entry (Dr Cash $100.00 / Cr Sales Revenue $100.00):
curl -X POST localhost:3000/journal-entries -H 'content-type: application/json' -d '{
  "entry_date": "2026-10-26", "memo": "Manual sale", "source": "manual",
  "lines": [
    {"account_code": "1000", "debit_cents": 10000},
    {"account_code": "4000", "credit_cents": 10000}
  ]}'
# Try an unbalanced one — the database itself refuses it (HTTP 400):
curl -X POST localhost:3000/journal-entries -H 'content-type: application/json' -d '{
  "entry_date": "2026-10-26", "memo": "One cent short", "source": "manual",
  "lines": [
    {"account_code": "1000", "debit_cents": 10000},
    {"account_code": "4000", "credit_cents": 9900}
  ]}'
```

Environment: `ERPCRAFT_DB` (default `erpcraft.db`), `PORT` (default `3000`).

## Hard rules (decided once, not revisited)

1. Money is never f64 — integer minor units everywhere.
2. The double-entry invariant is enforced in the DB layer.
3. The journal is append-only; corrections are reversing entries.
4. The agent never posts unconfirmed: propose → show the journal lines in
   plain language → human confirms → post.
5. Every command has a test; the oracle day's hand-computed trial balance is
   the acceptance suite.

## Layout

- `crates/erpcraft-core` — ledger core: schema + invariant triggers, CoA
  seed, posting, trial balance, tests.
- `crates/erpcraft-server` — Axum HTTP server over the core.

Out-of-scope ideas (procurement, MRP, payroll, multi-currency, tax engine)
live in [ROADMAP.md](ROADMAP.md), not in the build.

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE) at your option — the Rust ecosystem standard,
and the same terms as the ArtCraft suite.

Built for the Meta Global AI Developer Hackathon (Oct 26 – Nov 4, 2026),
Indie Dev Track, theme *Build for Your World*.
