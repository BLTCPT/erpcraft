# CLAUDE.md — working on ERPCraft

ERPCraft is a free, open-source, clean-room ERP in Rust, AI-native. This file
is the contract for any AI contributor (Claude, fleet agents) working here.
`AGENTS.md` carries the same rules for other agent tooling.

## Commands

```sh
export PATH="$HOME/.cargo/bin:$PATH"   # toolchain is not on the default PATH on the dev VM
cargo test                             # full workspace test suite — must be green before any commit
cargo run -p erpcraft-server           # HTTP server on 127.0.0.1:3000 (PORT / ERPCRAFT_DB env)
```

## Hard rules — never violate, never "fix" by weakening

1. **Money is integer cents (`Cents` = i64). Never f64.** A float near money
   is a reject-on-sight bug.
2. **The double-entry invariant lives in the database.** The
   `trg_entry_post_balance` trigger in `erpcraft-core` is the enforcer. Do
   not bypass it, disable triggers, or re-implement balance checks only in
   Rust to "make a test pass." If a posting is rejected, the posting is wrong.
3. **The journal is append-only.** No UPDATE/DELETE paths for posted entries
   or lines — the triggers forbid them and so do we. Corrections are new
   reversing entries with `reverses_entry_id` set.
4. **The agent never posts unconfirmed.** Agent flows propose postings, show
   journal lines in plain language, and post only after a human confirms.
5. **Every command gets a test.** The oracle day (scripted business day,
   hand-computed ending trial balance $129,975.00 both sides) is the
   acceptance suite for the posting engine — Day 3.

## Scope gates

In scope: ledger, order-to-cash (quote → order → invoice → payment), audit
trail, CLI + MCP + JSON channel, natural-language agent.
Out of scope (park in ROADMAP.md, do not build): procurement, MRP, payroll,
multi-currency, tax engine, multi-tenancy. Scope cuts — when time is short —
come from agent polish or UI, never from ledger invariants.

## Demo data (frozen)

Demo company: **Cascade Precision Parts**. Customers: Acme / Beta / Delta /
Epsilon. Standard cost **$68.00/unit** (6800 cents) — the oracle day's
arithmetic is tied to it; changing it means recomputing the oracle by hand
and updating the spec, not just the constant. Opening trial balance:
$120,500.00 per side, posted as the seed's opening journal entry.

## Style

- Rust 2021, `cargo fmt` defaults, no clippy warnings knowingly left behind.
- Commit messages: imperative, one logical change each.
- README honesty: this is an early alpha with one provably-correct loop.
  Big vision, honest status, verifiable proof — never oversell.
