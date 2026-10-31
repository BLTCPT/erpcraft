# ENTERPRISE_GAPS.md — what Meta- and Lam-Research-scale companies need that ERPCraft does not have (yet)

Written Oct 31, 2026, against the build as it stands: double-entry ledger
core (DB-trigger-enforced), order-to-cash with a GL posting engine, a
typed SAP LO-VC-style variant configurator, an 85-command CLI + MCP
dispatch table, an NL agent behind a propose→confirm gate, a Day-6
audit trail, and dynamic MRP on the Warp Speed pattern. 165 tests,
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
bank account model, no statement import, no reconciliation workflow —
a payment is "correct" because the journal says so, never because a
bank agreed.

**Real:** bank accounts as first-class objects with statement import
(CAMT.053/BAI2/CSV), automatic matching of statement lines to
payments and receipts, outstanding-item aging, bank fees and interest
posted from the statement, and multi-bank cash positioning. At Meta
and Lam scale this is a dedicated treasury function with payment
factories and in-house-bank structures on top.

**Gap shape:** additive — a bank-account table, statement lines, and
a matching engine against payments/receipts; the ledger core is
untouched. **Size: M.**

## 9. Inventory costing & valuation depth

**Today:** one valuation rule — standard cost, snapshotted onto
invoice lines at issuance. Quantities live in MRP's `stock_levels`;
there is no movement history (who moved what, when, against which
document), no lot/serial tracking, no cycle counting, and receipts
value at PO price into a single inventory account per make/buy class.

**Real:** moving-average and FIFO layers per item/site, landed-cost
apportionment (freight, duty), lot and serial traceability for
regulated parts, consignment and subcontracting stock, cycle-count
programs with variance posting, and a complete goods-movement ledger
as the subledger the GL inventory balance must reconcile to.

**Gap shape:** a movement table (the inventory analogue of the
journal: append-only, document-referenced) plus a costing engine.
The journal pattern transfers directly; the volume of edge cases is
the work. **Size: L.**

## 10. Manufacturing depth beyond MRP

**Today:** MRP nets demand and explodes BOMs with low-level codes
and lead-time offsets; release creates production/purchase orders;
completion is a value-only builder. No capacity model, no lot
sizing, no safety stock, no scrap/yield factors, no finite
scheduling — the design doc names these limits openly. One
structural wart: procurement's purchase orders are a *second*
order book, invisible to MRP's supply picture — an item already
ordered through P2P is planned again by MRP until it is received.
The two supply models must unify.

**Real:** work-center capacity and finite-capacity scheduling,
lot-sizing rules (EOQ, period-order quantity), safety-stock
policies by service level, planned scrap, co-products/by-products,
and — Lam's world — long-cycle engineer-to-order builds that sit in
WIP for months with progress billing and milestone recognition.

**Gap shape:** additive planning engines over the same demand and
BOM data; the netting core (fixed and probe-tested Oct 31) is the
foundation, not the obstacle. **Size: L per engine.**

## 11. Fixed assets & depreciation

**Today:** a `record_depreciation` builder posts a manual two-line
entry. There is no asset register, no capitalization workflow, no
depreciation schedule — the number comes from whoever runs the
command.

**Real:** an asset subledger (acquisition, transfers, retirements),
parallel depreciation books (GAAP, tax, local statutory) with
different lives and methods, capex approval workflows, and automatic
monthly depreciation runs whose every line traces to an asset.

**Gap shape:** a classic subledger build — register + schedule
engine + run posting through the one posting path. **Size: M.**

## 12. Payroll & people costs

**Today:** nothing — direct labor is a manual journal builder
(`record_labor`) fed by a number from outside the system.

**Real:** payroll is usually a specialized system (Workday/ADP) even
at companies that build everything else, because the rules are
jurisdictional and change constantly: withholdings, benefits,
equity compensation, multi-country payroll. What the ERP must own
is the *posting interface*: labor distributions into cost centers
and WIP, accruals, and reconciliation to the payroll provider's
totals.

**Gap shape:** deliberately an integration, not a build — a labor
import that posts distributions through the approval-gated paths.
**Size: S–M, and a build-vs-integrate decision to make on purpose.**

## 13. Financial reporting, close & statutory export

**Today:** a trial balance, per-account ledgers, and the audit
trail. No financial statements (balance sheet / P&L / cash flow as
presentations), no close checklist or close calendar workflow
beyond fiscal-period gating, no statutory exports.

**Real:** statement presentation with comparative periods and
drill-down to journal lines, a managed close (task lists, preparer/
reviewer sign-off, late-posting controls), consolidation-ready
reporting, and machine-readable statutory files — SAF-T in the
EU/OECD world, CSV/XML audit extracts everywhere. A swappable,
mappable chart of accounts belongs here too: today the chart is a
seed constant.

**Gap shape:** reporting is read-only over existing data (S–M);
close workflow and export formats are additive (M); chart mapping
touches the account model (M). **Size: M overall.**

## 14. Integrations & API maturity

**Today:** four doors over one dispatch table (CLI, MCP, HTTP,
agent) with per-command schemas. Idempotency exists exactly once —
the supplier-invoice reference guard added Oct 31 — and nowhere
else: a retried HTTP call can double-post wherever a human gate
doesn't intervene. No webhooks or event stream, no bulk import, no
EDI, no versioning policy.

**Real:** idempotency keys on every mutating call, an event/outbox
stream other systems subscribe to, bulk and EDI channels (suppliers
and customers at Lam scale do not type into portals), API
versioning with deprecation windows, and rate/tenant controls.

**Gap shape:** the dispatch table is the right seam — idempotency
and events can be added once, centrally, rather than per command.
**Size: M.**

## 15. Concurrency, scale & recovery

**Today:** one SQLite file, one writer; the HTTP server serializes
through a mutex (a locked database would rather queue than
corrupt). No backups story, no replication, no point-in-time
recovery beyond the file itself — acceptable for a single-company
pilot, disqualifying beyond it.

**Real:** concurrent writers with row-level contention only where
it matters, hot backups and PITR, read replicas for reporting,
and an upgrade/migration discipline across versions. The honest
path is an engine change (Postgres-class) behind the core's
existing connection abstraction — the domain code speaks SQL in a
small number of places, but the trigger-enforced invariants must
be re-proven on the new engine, not assumed.

**Gap shape:** the invariants are the migration risk, not the
queries. **Size: L–XL.**

## 16. Document lifecycle depth

**Today:** the O2C/P2P chains are complete but linear — no partial
shipments or backorders, no credit memos as first-class documents
(a return is the correction path; void is draft-only by design
since Oct 31), no recurring billing, no dunning, no disputes
workflow. Sales returns and AP overrides carry the correction
load.

**Real:** split shipments and backorder management, credit/debit
memos with their own approval chains, installment and milestone
billing for long-cycle equipment, subscription/recurring revenue
schedules (tying into §4), and collections tooling.

**Gap shape:** each is a new document type on the established
state-machine + posting pattern. Also here: reversal linkage is
today a bare (validated-as-existing, nothing more) foreign key —
a formal reversal model, with link validation and
reversal-of-reversal rules, belongs to this item. **Size: M each.**

## 17. Agent-control depth

**Today:** the propose→confirm gate, one-shot confirmation
fingerprints, confirm-time revalidation, prompt-attributed
overrides, and a full audit trail — probe-tested Oct 31. What is
missing: *authenticated* confirmation (the gate proves a human
was present, not who), per-agent scopes and budgets (an agent
that may invoice may not pay), and proposal isolation — a
proposal currently builds its draft document chain in the open
(an accepted quote and a fulfilled order exist before the human
decides; cancelling the proposal leaves them, by design of the
underlying state machines, though nothing posts).

**Real:** signed/attested approvals tied to the identity system
of §7, policy-as-data (amount limits per agent per intent,
time-of-day and counterparty rules), dry-run sandbox isolation for
proposals with atomic discard, and fleet-level kill switches.

**Gap shape:** identity first (depends on §7's full RBAC), then
policy tables the agent checks at propose time. **Size: L.**

---

## Build order this document argues for

1. **Done in the sprint:** procure-to-pay (§3), fiscal periods
   (§6 foundation), approval rule (§7 foundation).
2. **Multi-currency & FX (§2)** — the first gap that blocks real
   companies rather than merely inconveniencing them.
3. **Tax engine slot (§5)** — designed as a slot at the posting
   choke point, built jurisdiction by jurisdiction.
4. **Bank reconciliation (§8)** — the CFO's daily proof that the
   books match the bank.
5. **Reporting, close & statutory export (§13)** — read-mostly,
   high-trust value.
6. **Inventory costing (§9), then manufacturing depth (§10).**
7. **Fixed assets (§11); payroll as integration (§12).**
8. **API maturity (§14) and agent-control depth (§17)** — as the
   doors gain real external users.
9. **Scale & recovery (§15)** — when a second writer is a real
   requirement, not a fear.
10. **Multi-entity consolidation (§1)** — last because it is the
    most invasive change in the document: every table, every
    posting, every report. It is designed-for from here on
    (nothing above may assume one company in a way that blocks
    it), but it is built when a second entity actually exists.
