# ROADMAP — parked ideas (not in the hackathon build)

The sprint builds one loop, provably correct: ledger + order-to-cash +
audit trail + agent. Everything below is deliberately out of scope for the
10-day build; new ideas land here, not in the code.

## Landed during the sprint (no longer parked)

- **Variant configurator** — shipped Oct 27–28 as the headline
  differentiator: typed LO-VC-style dependencies with a deterministic
  evaluator, roll-up pricing, AI authoring/explanation behind the
  gate. Design: `CONFIGURATOR_DESIGN.md`.
- **Audit trail** — shipped Oct 31 (Day 6): full entry provenance,
  chains, and agent prompts. See the README.
- **MRP** — shipped Oct 31 (Day 7) on the Warp Speed pattern:
  dynamic, explainable, agent-native planning with gated release.
  Design + honest limits: `MRP_DESIGN.md`. (Capacity, safety stock,
  lot sizing, and supplier masters remain parked below.)
- **Recorded-response demo mode** — shipped; the T1 fixture was
  re-recorded from the live Baseten model on Oct 31.

## ERP breadth (post-hackathon)

- Procure-to-pay depth: supplier masters, AP matching (MRP purchase
  orders exist as planning documents; the purchasing ledger loop is
  still the Day-3 builders)
- Inventory depth: lots/serials, multi-warehouse, reorder points
- Manufacturing depth: capacity planning, safety stock, lot sizing,
  multi-level pegging reports (single-run MRP shipped in the sprint)
- Payroll and HR
- Multi-currency with FX gain/loss postings
- Tax engine (the seed carries a Sales Tax Payable account, held at $0)
- Period close, fiscal calendars, retained-earnings roll-forward automation
- Financial statements: balance sheet / income statement rendering, exports

## Platform

- Multi-tenant hosting and user/role permissions
- WASM build (the Storytold pattern: native + web from one codebase)
- Import/migration tooling from incumbent ERPs (CSV/Excel onboarding)
- Plugin/module system so verticals (job shops, distributors, services)
  extend the core without forking the ledger

## Agent surface

- Batch confirmations with risk tiers (auto-post under a threshold once a
  calibration gate earns it — never by default)
- More natural-language coverage: purchasing, payments runs, month-end
  accruals — each behind the same propose → confirm → post gate
