# ENTERPRISE_GAPS.md — what Meta- and Lam-Research-scale companies need that ERPCraft does not have (yet)

Written Oct 31, 2026, against the build as it stands: double-entry ledger
core (DB-trigger-enforced), order-to-cash with a GL posting engine, a
typed SAP LO-VC-style variant configurator, a 58-command CLI + MCP
dispatch table, an NL agent behind a propose→confirm gate, a Day-6
audit trail, and dynamic MRP on the Warp Speed pattern. 92+ tests,
single company, single currency, single SQLite node, demo data for one
fictional precision-parts maker (Cascade Precision Parts).

This is a roadmap artifact, not marketing. Every entry says what we
have today — honestly, including "nothing" — what "real" looks like at
a company like Meta (≈$165B revenue, ~120 legal entities, 100+
countries) or Lam Research (≈$18B revenue, long-cycle configured
capital equipment, a large installed-base service business), and a
rough build size (S ≈ a day, M ≈ a few days, L ≈ a week+, XL ≈ a
project of its own) in this codebase's idiom. The ranking at the end
is the build order this document argues for.

Scale anchors used throughout: SAP S/4HANA and Oracle Fusion are the
incumbents at both companies; Lam runs its configure-to-order business
on SAP with LO-VC (the direct ancestor of our configurator) and
variant-heavy ETO builds that sit in WIP for months; Meta runs
hyperscale data-center capex, a global ads business with complex
revenue and tax treatment, and SOX controls over everything.

---

## 1. Multi-entity / multi-company consolidation & intercompany

**Today:** nothing. One company, one chart of accounts, one ledger.
`DEMO_COMPANY` is a constant; there is no company dimension on any
table.

**Real:** legal-entity ledgers per subsidiary with a group chart of
accounts mapped to local charts; intercompany sales/purchases that
auto-generate the mirror document in the counterparty entity and
eliminate on consolidation; consolidation with minority interest,
equity pickup, and consolidation journals; entity-level close
calendars that roll up. Meta consolidates 100+ entities monthly;
Lam's intercompany flows (US parent → Asia manufacturing/service
subsidiaries) are a first-class workload, not an edge case.

**Gap shape:** this is a data-model change (a `company_id` on every
document and journal line, or schema-per-entity) plus a consolidation
engine. It touches everything, which is why it ranks below flows that
can be built additively now. **Size: XL.**

## 2. Multi-currency & FX revaluation

**Today:** nothing. `Cents` is unit-less; every amount is implicitly
USD. There is no currency code, no rate table, no revaluation.

**Real:** transaction currency vs. functional currency vs. group
currency on every posting; daily/period-end rates; realized and
unrealized FX gain/loss on settlement and period-end revaluation of
open AR/AP/cash balances (FAS 52 / ASC 830); translation adjustments
to CTA on consolidation. Lam sells in EUR/JPY/KRW/TWD and buys
globally; Meta's exposure is larger still. Note the interesting
tension with house rule #1: rates are the one place a decimal (never
a float — a fixed-point rate type) must enter the system, while
amounts stay integer cents per currency.

**Gap shape:** currency code on documents + monetary accounts, a rate
table, a revaluation run that posts adjustment entries (our
append-only, reversing-entry machinery fits this well). **Size: L.**

## 3. Procure-to-pay depth

**Today (before this weekend's build):** nothing but two thin journal
builders (`buy_materials` Dr 1200 / Cr 2000, `pay_supplier` Dr 2000 /
Cr 1000) — raw postings with no documents, no suppliers, no match.
MRP's planning `purchase_orders` table is a quantity plan, not a
payable. The weekend build closes the core of this gap: suppliers,
requisitions → purchase orders → goods receipts → AP invoices with
three-way match → AP payments, GR-IR clearing, posting inside the
transitions. What remains after it is listed in §17.

**Real:** everything the weekend build adds, plus: supplier master
data with banking/tax/terms, catalogs and punchout, blanket POs and
contracts, receiving inspections (see §13), debit/credit memos,
self-billing, payment terms and discount optimization (2/10 net 30),
payment runs/batching, 1099/withholding. Both companies run P2P at a
scale where three-way match exception queues are a staffed function.

**Size:** core flow M (built Oct 31); the long tail L.

## 4. Revenue recognition (ASC 606)

**Today:** partial, and naïve. Revenue posts in full at invoice
issuance (Dr AR / Cr 4000) — correct for a point-in-time goods sale,
which is all the demo models. Customer deposits sit in 2200 but
nothing relieves them against revenue automatically. There are no
performance obligations, no deferred revenue schedule, no milestone
or percentage-of-completion recognition, no service-contract revenue.

**Real:** ASC 606 five-step model — identify performance obligations,
allocate transaction price (incl. variable consideration and
constraints), recognize on satisfaction: over time for services and
long-cycle builds (Lam's tool installations and service contracts,
Meta's… mostly point-in-time ads, but its hardware/RL and
enterprise arrangements differ), deferred-revenue waterfalls,
contract assets/liabilities, and disclosure reporting. For Lam this
interlocks with §12: an ETO tool's revenue may recognize across a
months-long build-and-accept cycle, not at shipment.

**Gap shape:** a contract/obligation model on top of invoices plus a
recognition scheduler that posts periodic entries (our posting
engine's shape fits). **Size: L.**

## 5. Tax engine

**Today:** almost nothing. Account 2100 Sales Tax Payable exists in
the chart and stays at zero forever — no transaction ever posts to
it. No tax codes, no jurisdiction logic, no VAT/GST, no e-invoicing.

**Real:** determination (what tax, which jurisdiction, which rate —
Avalara/Vertex-class engines exist because this is genuinely hard),
US sales/use tax nexus, EU VAT with reverse charge and Intrastat,
GST regimes across Lam's Asia footprint, withholding taxes,
e-invoicing mandates (EU ViDA, Latin America clearance models),
exemption certificates, and tax reporting/returns. Meta's digital-
services taxes and Lam's cross-border equipment/service flows both
live here.

**Gap shape:** a tax-code/jurisdiction model hooked into invoice and
AP posting (the posting engine has clean insertion points), with
determination behind an interface so an external engine can be
plugged in. **Size: L** (determination content itself is effectively
unbounded — the interface is the buildable part).

## 6. Period close & fiscal calendar

**Today (before the weekend build):** nothing. Entries can be posted
with any date, forever; there is no period, no close, no calendar.
The weekend build adds the foundation: fiscal periods and a close
that refuses postings dated into a closed period, on every door.

**Real:** 4-4-5 or custom fiscal calendars, soft vs. hard close,
close checklists and task orchestration, accrual/deferral runs,
revaluation (§2) and consolidation (§1) as close steps, prior-period
adjustment controls, and a close calendar that differs per entity.
Both companies close monthly under SOX timelines; the close is a
managed process, not a status flag.

**Size:** foundation S–M (built Oct 31); close orchestration M.

## 7. Identity, RBAC, segregation of duties, approvals (SOX)

**Today (before the weekend build):** nothing. There are no users.
The audit trail records *which door* posted (cli/mcp/agent/engine)
and the verbatim prompt for agent work, but no human identity, no
roles, no permissions, and no approval workflow. The agent's
propose→confirm gate is a human-in-the-loop control, but the human
is anonymous. The weekend build adds the foundation: a users/roles
model and an approval rule (payments above a configurable threshold
need a second, distinct approver whose identity lands on the
payment's journal entry, hence on its audit record).

**Real:** SSO/SCIM identity, role-based permissions per module and
per entity, segregation-of-duties rules (the person who creates a
supplier cannot approve its payment; requester ≠ receiver ≠ payer),
delegation and approval chains by amount, periodic access reviews,
and SOX-grade evidence that all of it was enforced — i.e., the
audit trail must be able to show *who was allowed*, not just who
acted. This is table stakes for any public company; it is also the
gap a CFO notices first.

**Size:** foundation M (built Oct 31); full RBAC enforcement across
every command L.

## 8. Treasury & bank reconciliation

**Today:** nothing. Cash is account 1000 with a balance; there is no
bank account model, no statement import,
...[truncated 8926 chars]