# ERPCraft — Hackathon Submission

*Meta Global AI Developer Hackathon · Indie Dev Track · Team: Thalo Works*
*Theme: Build for Your World*
*Deadline: Wednesday, November 4, 2026, 6:00 PM PT*

---

## Project name

ERPCraft

## Tagline

A free, open-source, clean-room ERP in Rust — AI-native from day one, with a
double-entry ledger the database itself refuses to let you break.

## What it does

ERPCraft is an enterprise resource planning system for the world its builder
actually lives in: small manufacturers who run their businesses on
spreadsheets because the incumbent ERPs are closed, expensive, and held
together by consultants — and who are now being asked to trust AI agents with
real books.

So ERPCraft is built around one idea: **an AI agent should drive the exact
same commands a human does, propose before it posts, and work on top of a
ledger that is provably correct even if the agent — or anyone — misbehaves.**

The vision is the full ERP: general ledger, order-to-cash, purchasing,
inventory, manufacturing planning, and reporting, owned by the people who run
it. This repository is the working core of that vision, built and verified in
the open:

- **A double-entry ledger enforced by the database itself.** Every posting
  must balance (debits = credits) — the rule lives in SQLite triggers, not
  application code, so no path (HTTP, CLI, MCP, the AI agent, or raw SQL)
  can post an unbalanced entry. The journal is append-only; corrections are
  reversing entries that reference the original. Money is integer cents,
  never floating point.
- **Order-to-cash with a GL posting engine.** Quotes → sales orders →
  invoices → payments → returns, where the transitions themselves generate
  the revenue, COGS, payment, and reversal postings. Correctness is pinned
  by a hand-computed "oracle day": 14 transactions whose ending trial
  balance ($129,975.00 per side) is the acceptance suite.
- **A variant configurator modeled on SAP LO-VC object dependencies** —
  precondition, selection condition, and constraint — as a typed AST,
  evaluated deterministically in the core. Selected components roll into
  line price *and* standard cost. AI works only at the edges: it authors
  dependency rules behind a propose → confirm gate and explains refusals in
  plain language. Ask for a galvanized finish on an aluminum bracket and
  the system refuses, naming the rule: *"Galvanized finish is available
  only on steel brackets."*
- **One dispatch table, four doors.** 58 commands served identically to an
  operator CLI, an MCP server on stdio, an HTTP API, and the natural-language
  agent — JSON out, plain-language refusals, exit codes 0/1/2. Anything a
  human can do, an agent can drive; there is no second, weaker API.
- **A natural-language agent behind a propose → confirm gate.** Plain
  English ("Invoice Acme for the 40 units we shipped Tuesday") becomes
  *proposed* dispatch calls. The confirmation is rendered from actual ledger
  data — never from model prose — and nothing posts until a human confirms.
  The agent also answers questions and can propose releasing a plan.
- **AI-native MRP on the Warp Speed pattern.** Materials planning that is
  *dynamic* (every run recomputes from current orders, stock, BOMs, and
  configurations — plans are never stored, so a changed order re-plans
  instantly), *explainable* (every planned order carries its netting math
  and demand reasons; a shortage is simply a planned order whose start date
  has already passed), and *agent-native* ("Can we build 200 BRK-100 by
  Nov 15?" is answered from a live run; releasing a planned order goes
  through the same confirm gate and creates the document — planning never
  posts to the GL).
- **A full audit trail.** Every journal entry explains itself: who posted
  it, which document it belongs to, why it posted, the complete provenance
  chain (quote → order → invoice → payments → returns) with reversal links
  in both directions — and, for agent postings, the verbatim human prompt
  that proposed it.

The demo company is Cascade Precision Parts, a fictional small Oregon
manufacturer making metal brackets.

## How it was built

Pure Rust, three crates in one workspace:

- `erpcraft-core` — the deterministic core: ledger schema and invariant
  triggers, the posting engine, order-to-cash, the variant configurator,
  the audit trail, and MRP. The evaluator and the planner are pure
  functions of current state.
- `erpcraft-server` — an Axum HTTP API over the core.
- `erpcraft-cli` — the dispatch table (CLI + MCP doors) and the
  natural-language agent adapter over it.

Design rules decided once and held throughout: integer cents everywhere;
double-entry enforced in the DB layer; the journal is append-only and
corrections are reversing entries; agents propose, humans confirm, then it
posts; deterministic core with AI at the edges (models parse, author behind
the gate, and explain — they never evaluate the configurator or compute a
plan); every command has a test, and the hand-computed oracle-day trial
balance is the acceptance suite. 95 automated tests are green.

The AI author behind this project works professionally in SAP variant
configuration (LO-VC) — the configurator's dependency semantics come from
that world experience, re-implemented clean-room as typed data rather than
stringly-typed syntax.

## Partner technology

- **Baseten — Muse Spark Open (`meta-models/muse-spark-open-1026`), used
  live.** The agent's live parse route runs on Baseten (hackathon grant).
  The shipped demo fixture was recorded from the actual model (schema-valid
  output), and the payment and price-conflict prompts were spot-checked
  live and replayed through the real agent binary with identical outcomes.
  Total live usage: ~5,600 tokens against the grant.
- **Meta Model API — wired as a route.** The agent's client supports the
  Meta Model API (`--client meta`) alongside Baseten, the recorded fixture,
  and a deterministic offline stub, which remains the default so the demo
  and the test suite run with no network at all.

## What to try (5-minute demo)

From the repo root, with a fresh database — the full script is in the
README:

```sh
export ERPCRAFT_DB=/tmp/demo.db; rm -f $ERPCRAFT_DB
E="cargo run -p erpcraft-cli --"
```

1. `$E health` and `$E trial_balance` — the seeded company; opening books
   balance at $120,500.00 per side.
2. Configure a quote: a steel BRK-100 with a galvanized finish
   (`$E create_quote --json '{...}'`, then send/accept/order). The
   configurator forces the reinforced mount via a named constraint and
   prices the line at $162.00/unit. Then ask for aluminum + galvanized via
   `evaluate_config` — it refuses, with its reason.
3. `$E agent "Invoice Acme for the 40 units we shipped Tuesday."` — the
   agent proposes; the confirmation shows the posting rendered from the
   ledger (total debits $7,720.00 = total credits $7,720.00). Nothing posts
   until you confirm.
4. `$E agent "Can we build 200 BRK-100 by 2026-11-15?"` — a live MRP plan:
   demand 250, 50 covered, build 200 starting Nov 10, with component
   purchases netted (30 steel blanks, 110 fastener sets). Push the as-of
   date past a start date with `explain_shortage` and the same plan
   explains the shortage instead.
5. Release the fastener purchase through the agent — propose → confirm
   creates the purchase-order document; the trial balance does not move.
   Planning never posts.
6. `$E agent --yes "Buy $3,600 of raw materials on credit."`, then
   `$E audit_trail --source agent` and `$E audit_entry <id>` — who posted
   each entry, why, the verbatim prompt, and the full provenance chain.

Also worth trying: `cargo test` (the invariant suite, including the oracle
day), posting an *unbalanced* journal entry over HTTP and watching the
database itself refuse it (HTTP 400), and `erpcraft mcp` to drive the same
58 commands as MCP tools.

## Repository

https://github.com/BLTCPT/erpcraft — public, dual-licensed MIT / Apache-2.0.

## Honest limits

- **MRP scope:** no capacity planning, safety stock, or lot-sizing yet —
  the planner nets quantities and offsets lead times; those three are the
  documented next increments (see ROADMAP.md).
- **One-off prices:** quote lines snapshot the item master's price;
  a one-off unit price on a single line is not yet supported. The agent
  detects the request, flags the conflict, and refuses rather than posting
  a wrong price.
- **Early alpha:** this is the working core of the ERP vision — ledger,
  order-to-cash, configurator, agent surface, MRP, audit — not yet the
  purchasing, payroll, tax, and multi-currency depth the full vision
  includes. Those are parked, by design, in ROADMAP.md; the invariants
  they will stand on are the part that had to be right first, and they are
  the part that is test-proven here.

## Team

Thalo Works — solo indie build (Mark Zellner) with an AI pair-programming
workflow, for the Indie Dev Track.
