# AGENTS.md — working on ERPCraft

ERPCraft is a free, open-source, clean-room ERP in Rust, AI-native: a
double-entry ledger core, the order-to-cash loop, and an agent surface
(HTTP today; CLI + MCP during the sprint) where every action a human can
take is a command an agent can drive — with a human confirmation gate
before anything posts.

## Commands

```sh
export PATH="$HOME/.cargo/bin:$PATH"   # cargo/rustc 1.99.1 live here on the dev VM
cargo test                             # workspace suite — green before commit, no exceptions
cargo run -p erpcraft-server           # 127.0.0.1:3000; env: PORT, ERPCRAFT_DB
```

## Non-negotiable invariants

1. Money = integer cents (`Cents`). Never floating point.
2. Double-entry is enforced by SQLite triggers in `crates/erpcraft-core`
   (an entry posts only if debits == credits, ≥ 2 lines, positive total).
   The DB is the enforcer; application checks are a courtesy layer.
3. Journal is append-only. Corrections = reversing entries referencing the
   original (`reverses_entry_id`). Never edit or delete a posting.
4. Agents propose; humans confirm; then it posts.
5. Tests accompany every command. Oracle day ends at a hand-computed trial
   balance of $129,975.00 per side.

## Frozen demo data

Cascade Precision Parts; customers Acme / Beta / Delta / Epsilon; standard
cost $68.00/unit; opening trial balance $120,500.00 per side.

## Scope

Build: ledger, order-to-cash, audit trail, CLI/MCP/JSON, NL agent.
Park in ROADMAP.md instead of building: procurement, MRP, payroll,
multi-currency, tax engine, multi-tenancy, hosted deployment.
When cutting scope, cut agent polish or UI — never ledger invariants.
