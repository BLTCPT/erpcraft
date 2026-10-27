# ROADMAP — parked ideas (not in the hackathon build)

The sprint builds one loop, provably correct: ledger + order-to-cash +
audit trail + agent. Everything below is deliberately out of scope for the
10-day build; new ideas land here, not in the code.

## ERP breadth (post-hackathon)

- Procure-to-pay: suppliers, purchase orders, receiving, AP matching
- Inventory depth: lots/serials, multi-warehouse, reorder points
- Manufacturing: BOMs, work orders, MRP, capacity
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
- Recorded-response demo mode for offline presentations
- More natural-language coverage: purchasing, payments runs, month-end
  accruals — each behind the same propose → confirm → post gate
