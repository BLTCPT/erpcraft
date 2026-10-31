# LIVE_EVAL.md — the agent, evaluated against the live model

*2026-10-31 · Baseten grant route (`meta-models/muse-spark-open-1026`) ·
Mark's directive: spend the credits we actually got; do not ration runs.*

Until today, the live route had been exercised on three prompts (T1
re-recorded; T3/T11 spot-checked). This pass ran **every** acceptance
scenario — the 12 contract prompts plus the suite's extras, both MRP
intents, the configurator what-if path, and the two new P2P intents —
through the live model, recorded the replies, and replayed each one
through the real binary. **All 23 scenarios replay to exactly the
contract outcomes** (identical postings, refusals, clarifications and
balances as the deterministic stub suite): `cargo test -p erpcraft-cli
--test agent_live_replay` — 23 passed / 0 failed.

## Method

1. **Capture** (`crates/erpcraft-cli/examples/live_eval.rs`): each
   scenario's ledger state is built exactly as the acceptance suites
   build it, the *real* adapter runs with a capturing client, and the
   exact `(system, user)` messages the live route would send are
   written to `live-eval/requests.json`. Nothing is reconstructed.
2. **Call** (`live-eval/drive.py`): the requests go to Baseten through
   the `custom.baseten` connector route (authd surrogate; the key
   never appears in this repo), `temperature: 0`, generous
   `max_tokens`. Raw responses are kept in `live-eval/responses/`.
3. **Record**: replies are assembled into
   `crates/erpcraft-cli/fixtures/recorded_live_baseten.json` (21
   exchanges; two prompts are shared by two scenarios each — T1
   fresh/duplicate and "Pay AP invoice 1." matched/held — first
   recording kept, twins preserved in `responses/` and compared).
4. **Replay** (`crates/erpcraft-cli/tests/agent_live_replay.rs`):
   the RecordedClient feeds each live reply to the real adapter on
   the scenario's state, and the full contract assertions run —
   the gate, the postings to the cent, the refusal texts, the
   balances.

## Which money was real (credit audit summary)

- **Baseten $100 — granted, and this eval ran on it.** Route verified
  live (HTTP 200 on the grant model) before the sweeps.
- **Meta Model API $150 — NOT spent.** The key verifies (model list
  answers), but no evidence exists that the $150 grant ever landed:
  no grant/credit email, and the connector exposes no balance signal;
  the claim was a separate form that also needs a card on file, so
  calls could bill a card. A valid key is not "money we got."
- **Fireworks $25** — no grant/selection email exists; still pending.
- **Modal $100** — claim never filed (the form needs a workspace
  name; the account-approval email of 2026-10-26 does not state one).

## Token spend (Baseten, this eval)

| Run | Tokens | Notes |
|---|---:|---|
| Sweep 1 (cap 4096) | 51,451 | 3 replies with **no content** — see divergence 1 |
| Sweep 2 (prompt fix 1, cap 8192) | 42,955 | all 23 answered |
| Sweep 3 (prompt fix 2, cap 8192) | 49,102 | 1 no-content reply (9,291 tokens) |
| Refill of that scenario | 1,482 | answered on the first retry |
| **Eval total** | **144,990** | plus 5,567 from the earlier T1/spot-check work ≈ **150.6k all-in** |

The shipped fixture's kept recordings total 41,293 tokens across
21 exchanges.

## Results by scenario

Every scenario's live parse and replay outcome (all green):

| Scenario | Live parse (essence) | Replay outcome |
|---|---|---|
| T1 invoice | `invoice_shipment` Acme ×40, Tuesday | proposal → posts $7,720.00 balanced |
| T2 payment | `record_payment` Delta "$3,125" | posts; balance $1,875.00, partially_paid |
| T3 deposit | `record_deposit` Epsilon "$1,500" | Dr Cash / Cr Customer Deposits — liability |
| T4 return | `record_return` Delta ×2 | reversing entries point at the original sale |
| T5 pay supplier | `pay_supplier` "$4,100" | Dr AP / Cr Cash |
| T6 rent | `record_rent` "$1,200" | Dr Rent / Cr Cash |
| T7 missing qty | `invoice_shipment`, no qty | clarification, nothing created |
| T8 unknown customer | `invoice_shipment` "Globex" | refusal with the customer list |
| T8b name trap | customer "Acme Manufacturing" verbatim | "Did you mean Acme?" — never auto-matched |
| T9 duplicate | identical to T1's reply | duplicate warning, nothing posted |
| T10 production | `complete_production` ×50 "$4,100" | Dr Finished Goods / Cr WIP |
| T11 price conflict | `price_each` "$130" | price-conflict question; list-price path posts |
| Out of scope | `out_of_scope` | plain-language refusal (see divergence 4) |
| MRP check (build) | `mrp_check` BRK-100 ×200, 2026-11-15 | live-run answer: builds 150, shortages none |
| MRP check (shortage) | `mrp_check` ×200, 2026-10-20 | shortage named with its reasons |
| MRP check (plain) | `mrp_check` | live-run shortage report |
| MRP release | `mrp_release`, key verbatim | gated proposal; confirm creates the PO, no entry |
| MRP release (unknown) | key …:999 verbatim | refusal: no such planned order |
| MRP configured what-if | `mrp_check` + `{material: aluminum, finish: galvanized}` | core refusal (see below) |
| P2P receive | `receive_goods` PO 1, BRK-100 ×10 | gated; posts Dr Raw Materials / Cr GR-IR $600.00 |
| P2P pay (matched) | `pay_ap_invoice` #1 | gated; invoice paid |
| P2P pay (held) | `pay_ap_invoice` #1 (sweep 3) | adapter's hold refusal with variances |
| P2P pay (held + override) | reason + `overrider` "Dana" | two-step proposal; override recorded, then paid |

## The configurator explanation path

The aluminum + galvanized refusal was run both ways it can surface:

- **Through the agent** (above): the model extracted the
  configuration; the refusal text is the core evaluator's structured
  explanation, surfaced by the MRP what-if path — *"what-if
  configuration for BRK-100 refused: Galvanized finish is available
  only on steel brackets."* The wording is core data, not model
  prose.
- **Through the CLI door**, on a fresh database:
  `erpcraft evaluate_config --json '{"model_id":1,"configuration":{"material":"aluminum","finish":"galvanized"}}'`
  → stderr `the configuration is inconsistent: Galvanized finish is
  available only on steel brackets.`, exit 1 — the reason exactly
  once (the Day-6 dead-end fix holding at the door).

## Divergences — recorded verbatim, none papered over

1. **Reasoning-loop no-content replies (the real finding).** On some
   prompts the model falls into a self-checking loop — its reasoning
   trace repeats *"We should ensure we don't include \"order_id\". /
   We should output."* field after field — and consumes the entire
   completion budget without emitting an answer: sweep 1 lost T7
   (5,057 tokens), the MRP shortage check (5,149) and P2P pay
   (5,078) at a 4096 cap; sweep 3 lost the MRP build check at the
   8192 cap (9,291 tokens, `finish_reason: length`, content `null`).
   It strikes stochastically (~1–3 of 23 per sweep) even at
   temperature 0; a bigger budget only makes a loop more expensive.
   **Fixes shipped:** the system prompt now says to emit the JSON
   immediately without deliberating over omitted fields (sweep 2 was
   clean), and the live client's cap rose 4096 → 8192. **Production
   safety net (already built):** a no-content reply is a clean
   `AgentError::Model`, and with the recorded fixture configured the
   FallbackClient degrades to it — never a crash, never a silent
   post (spec test 12). The loop is a model behavior to route
   around, not a defect the ledger ever sees.
2. **Held-invoice parse (sweep 2):** with the AP invoice flagged
   `(ON THREE-WAY-MATCH HOLD)` in context, the model answered
   `{"intent":"unclear"}` for "Pay AP invoice 1." — the payment was
   still safely refused (as a generic couldn't-parse), but the
   precise hold refusal never fired. **Prompt defect, fixed:** the
   pay_ap_invoice rule now says to parse a payment the same way
   whether or not a hold is shown — the adapter surfaces holds
   itself. Sweep 3 parses `pay_ap_invoice` and the refusal carries
   the variances, as designed.
3. **Override reason wording:** the stub's deterministic parse
   trims the justification to "the price increase"; the live model
   returned `"the price increase was approved by Dana"` — the full
   clause, attribution included. It is recorded on the invoice
   verbatim alongside `overridden_by: "Dana"`. The contract (the
   stated justification is what lands on the invoice) holds; the
   live wording is arguably the better audit record.
4. **Out-of-scope topic wording:** the live model's topic was
   "create purchase order", so the rendered refusal reads *"I don't
   handle create purchase order yet."* where the stub's reads
   *"…purchase orders yet."* Cosmetic; the outcome and the can-do
   list are identical.
5. **Harmless extra fields:** several replies added a `"sku"` the
   intent doesn't use (T4, T8, T10). Unknown fields are ignored by
   the adapter's parser; no outcome moved.

## Fixes shipped with this eval

- System prompt: emit-immediately instruction (divergence 1) and
  the parse-through-a-hold clarification (divergence 2).
- `OpenAiChatClient` completion budget 4096 → 8192, with the
  measurement recorded in the code comment.
- Expected ledger outcomes were never touched to make a live reply
  pass — every fix is in the prompt or the client budget.

## Reproduce

```sh
cargo run -p erpcraft-cli --example live_eval -- live-eval/requests.json
python3 live-eval/drive.py            # spends Baseten grant tokens
python3 live-eval/refill.py <id>      # if a scenario returns no content
cargo test -p erpcraft-cli --test agent_live_replay
```
