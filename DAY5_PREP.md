# ERPCraft Day 5 prep — natural-language agent over the Day-4 dispatch table

Prepared Thu Oct 29 (evening) from the code as built after Day 4
(commits `e94f183`..`1e9a10e`, workspace suite **54 passed / 0 failed**
verified locally Oct 29 ~17:45 PT, oracle day still $129,975.00 per side).
Day 5 per the sprint plan (`~/workspace/your_files/erpcraft-hackathon-sprint-plan.md`):
**Natural-language agent via Meta Model API (Muse Spark): parse request →
propose postings → confirm → post.**
Exit criterion: "Invoice Acme for the 40 units shipped Tuesday" posts
correctly, with the confirmation step.

The binding contract is `~/workspace/your_files/erpcraft-agent-contract-spec.md`
(Oct 13): its 12 intents, 8 parsing rules, confirmation contract, and
12-prompt acceptance suite bind Day 5 exactly as written. This prep only
reconciles that spec against the **core + dispatch table as built**; where
they differ, the as-built shapes win and the spec's intent is preserved.

House rules hold: money is integer cents, DB triggers enforce, journal is
append-only, **the agent proposes / the human confirms / only then it posts**.
Cut agent polish before touching a ledger invariant. The demo fallback if
the model API is down is the CLI for the same command — never a silent
auto-post (spec test 12).

## The one architectural rule

The agent is a **thin adapter over the Day-4 dispatch table** — the same
seam as the Jev adapter. It parses a prompt into `(command, args)`, renders
the proposed effect for confirmation, and calls `dispatch(...)` only after
an explicit confirm. It never writes SQL, never computes its own balances,
and never calls core functions the dispatch table doesn't expose. If a
prompt can't be expressed as dispatch calls, it is out of scope and the
agent says so in plain language.

Where the agent lives: a new `agent` module in `erpcraft-cli` (or a fourth
crate only if the CLI module gets unwieldy) with the model client behind a
trait, so tests run against a stub/recorded client with no network.

## Intent map — spec intent → dispatch as built

| # | Spec intent | Dispatch path as built | Notes |
|---|---|---|---|
| 1 | Invoice customer for N shipped units | Chain: `create_quote` → `send_quote` → `accept_quote` → `create_order` → `fulfil_order` → `create_invoice` → **`issue_invoice`** | NOT one call — the spec's `ship_and_invoice` was split as built, deliberately (Day-4 prep). The confirmation gate sits **before `issue_invoice`**, the only step that posts. Everything before it is draft state. If a fulfilled order already exists for the customer, start at `create_invoice`. Prices/costs come from the item + line snapshots, never the prompt (rule 2). |
| 2 | Customer paid $X against invoice | `record_payment` (invoice_id, amount_cents) | Keyed by invoice, not customer — the agent must resolve "their invoice" via `list_invoices` / `list_payments_for_invoice`; if more than one open invoice matches, that's a clarification, not a guess. Overpayment is refused in core. |
| 3 | Buy $X raw materials on credit | `buy_materials` builder (Dr 1200 / Cr 2000) | Builder takes `amount_cents` + `entry_date` only — supplier name goes in the proposal text, not the posting. |
| 4 | Pay $X to supplier | `pay_supplier` builder (Dr 2000 / Cr 1000) | Same shape as #3. |
| 5 | Issue $X raw materials to production | `issue_materials` builder (Dr 1210 / Cr 1200) | Ready as built. |
| 6 | Pay $X wages for production work | `record_labor` builder (Dr 1210 / Cr 1000) | Ready as built. (This is direct labor to WIP per the oracle day — not a payroll engine; out-of-scope list still holds.) |
| 7 | Finished N brackets, cost $X | **GAP — no builder exists.** | See "The one gap" below. |
| 8 | Customer sent $X deposit | `record_deposit` builder (Dr 1000 / Cr 2200) | Deposit trap (test 3): the confirmation must label 2200 a **liability, not revenue**. Builder has no customer arg — customer lives in the proposal/memo only. |
| 9 | Customer returned N defective brackets | `record_return` (invoice_id, lines[{item_id, qty}]) | Posts both reversing entries with snapshotted price/cost and points each at the entry it corrects — rule 7 satisfied by core. Agent must resolve which invoice via `list_invoices`; ambiguous → clarify. |
| 10 | Record rent $X | `record_rent` builder (Dr 6000 / Cr 1000) | Ready as built. |
| 11 | Record loan interest $X | `record_interest` builder (Dr 6010 / Cr 1000) | Ready as built. |
| 12 | Record depreciation $X | `record_depreciation` builder (Dr 6020 / Cr 1310) | Ready as built. |

Reads for resolution + duplicate checks: `list_customers`, `list_items`,
`list_invoices`, `list_entries`, `get_invoice`, `trial_balance`.

## The one gap — intent 7 has no builder

The Day-4 builders cover intents 3, 4, 5, 6, 8, 10, 11, 12. Intent 7 —
"We finished 50 brackets today, cost $4,100" → Dr 1220 Finished Goods /
Cr 1210 WIP — was not built, and acceptance test 10 requires it.
Fix (minutes, same pattern as the other eight): add
`("complete_production", "Complete production", "1220", "1210")` to
`BUILDERS` in `crates/erpcraft-cli/src/commands.rs` with its
`builder_spec` arm (args: `amount_cents`, `entry_date`; units are
proposal text — the posting is value-only, exactly like the oracle day's
T7). Do this first on Day 5, with a dispatch test, before the agent work.

## Confirmation contract — rendered from dispatch results, not model prose

The spec's confirmation block (account **names**, amounts, the
"Total debits = total credits ✓" line, [Confirm] [Edit] [Cancel]) must be
rendered by the adapter from the proposed dispatch calls and core data
(price list, snapshots, account names from the CoA) — never from free-text
the model generates. The model proposes *which command with which args*;
the adapter computes and displays *what will post*. A model that narrates
its own journal lines can hallucinate a balanced-looking lie; a renderer
over the actual calls cannot.

For intent 1, the proposal shows the draft invoice (from `create_invoice`,
pre-issuance): lines, snapshotted price and cost, and the two entry pairs
issuance will post. Confirm → `issue_invoice`. Cancel → the draft stays
unissued; nothing has posted, so there is nothing to reverse.

## Audit fields — one decision the build must make explicitly

Spec rule 8: every posting carries who / what / when / **why (the verbatim
prompt)**. As built, `NewEntry` has `memo` + `source`; entries through the
dispatch doors are tagged `cli` / `mcp`, and engine postings keep core's
`invoice:<id>` / `payment:<id>` / `return:<id>` sources. There is **no
prompt field**, and Day 6 (audit trail UI) will want one.

Day-5 decision (make it once, in the build's first commit): agent-driven
postings carry `source: "agent"` where the door controls the tag (the
manual builders + `post_entry`), and the **memo carries the intent summary
plus the verbatim prompt** (e.g. `Invoice Acme — prompt: "Invoice Acme
for the 40 units we shipped Tuesday."`). Engine postings (issuance,
payment, return) keep core's source tags — the prompt linkage for those
lives on the draft documents the agent created, and Day 6 can join through
them. Do NOT add a schema column mid-sprint unless Day 6 prep asks for it;
memo + source is enough to demo rule 8 honestly.

## Parsing rules — where each one is enforced

1. **Integer cents, exact parse or rejection** — adapter-side, before any
   dispatch call. "$5,000.00" → 500000. Unparseable money = refusal.
2. **Prices from the price list, never the prompt** — adapter checks any
   prompt-stated price against `list_items`; conflict → test 11 flow
   (flag, ask, post nothing until answered). Core snapshots enforce the
   rest.
3. **Relative days resolve against the demo day and are shown in the
   confirmation** — adapter-side; the resolved date is passed explicitly
   as `entry_date` (builders) so the posting and the confirmation can
   never disagree. Ambiguous day → ask.
4. **Customer names match the customer list** — adapter resolves against
   `list_customers`; near-miss = clarification, unknown = refusal with
   the list shown (test 8). Seeded names **as built** are the short forms
   — `DEMO_CUSTOMERS = ["Acme", "Beta", "Delta", "Epsilon"]` in core —
   not the spec's "Acme Manufacturing / Beta Corp". The spec's test-8
   wording ("Acme Corp" as the near-miss example) still binds in spirit:
   with short seed names, "Acme Manufacturing" is itself the near-miss
   that must clarify, never auto-match.
5. **Quantities present and positive** — adapter refuses/clarifies
   (test 7) before building any call.
6. **Duplicates flagged, never double-posted** — adapter scans
   `list_entries` / `list_invoices` for same customer + amount + date
   before proposing; a hit requires the explicit "post it anyway"
   override (test 9). This is a warning gate, not a core invariant —
   core will happily post a second entry if told to, so the check must
   run on every agent path, including retries after a model timeout.
7. **Corrections reference their source** — core's `record_return`
   already links reversing entries; the adapter's job is only resolving
   the right invoice (see intent 9).
8. **Audit fields** — see the decision above.

Out-of-scope prompts (purchase orders, payroll, tax, multi-currency…)
get a plain-language refusal naming what the agent *can* do — never a
guessed posting.

## Model route — primary, fallback, recorded

- **Primary: Meta Model API (Muse Spark)** — the sprint plan's Day-5
  design and the $150 credit's purpose. Key verified live via secure
  storage (never in chat, never in the repo). Credit was filed Oct 26
  with a stated ≤2-business-day landing window, so it should be live —
  **verify the credit balance before the build starts**; if it hasn't
  landed, do not burn billable completions, go to the fallback.
- **Fallback: Baseten Muse Spark Open** — $100 granted and verified
  Oct 27; OpenAI-compatible endpoint; the key works **only** with the
  exact model name `meta-models/muse-spark-open-1026`. Same key-handling
  rule: secure storage only, opened when ready to copy straight in.
- **Demo fallback: recorded responses / CLI** — spec test 12. The agent
  module's stub client doubles as this: a recorded T1 exchange replays
  the demo with no network, and below that the operator drives the same
  dispatch commands through the CLI. Neither path may skip the
  confirmation gate.

Whichever route is live, the model only ever returns a structured
intent + arguments (JSON against the dispatch command schemas the CLI
already publishes). Free-text journal lines from the model are ignored
by construction — see the confirmation rule above.

## Acceptance — the spec's 12 tests, as built

All 12 prompts from the contract spec must pass before Day 5 closes,
run against the stub client (deterministic) and spot-checked against the
live model for tests 1, 3, and 11. Mapping notes:

- Tests 1–6, 8, 11, 12: adapter + dispatch as mapped above; nothing new
  in core.
- Test 7 (missing quantity): clarification, zero dispatch calls — assert
  no entry count change.
- Test 9 (duplicate): run T1, confirm, re-run T1 → warning, no second
  posting without the explicit override.
- Test 10 (production completion): passes only after the
  `complete_production` builder lands (see gap above).
- Test 3 (deposit trap): assert the proposal labels 2200 Customer
  Deposits a liability and that Sales Revenue is untouched after post.
- Test 4 (return): assert both reversing entries carry
  `reverses_entry_id` pointing at the T8 issuance entries.
- After the suite: re-run the full workspace tests + the oracle replay —
  the agent must not move the $129,975.00 trial balance on the oracle
  day except through the scripted, confirmed postings.

## Day 5 order of work

1. `complete_production` builder + dispatch test (closes the only gap).
2. Agent module skeleton: model-client trait, stub client, intent JSON
   schema = dispatch command schemas.
3. Adapter: parse → resolve (customers/items/invoices) → duplicate scan
   → render confirmation from the proposed calls → confirm → dispatch.
4. Wire the 12-prompt acceptance suite against the stub; green before
   any live-model call.
5. Verify Meta credit landed; live spot-check tests 1, 3, 11 on the
   primary route; record the T1 exchange for the demo fallback.
6. Full workspace suite + oracle replay green; push public, same drill
   as Days 1–4.

Scope guard for the weekend that follows: Day 6 is the audit-trail
surface (the memo/source decision above is what it builds on), Day 7 is
hardening. Nothing from those days gets pulled forward into Day 5.
