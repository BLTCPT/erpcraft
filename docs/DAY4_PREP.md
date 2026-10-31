# ERPCraft Day 4 prep — MCP server + CLI over the core as built

Prepared Wed Oct 28 (evening) from the code as built after Day 3
(commit `5ef6789`, workspace suite 32 passed / 0 failed, oracle day at
$129,975.00 per side) and the configurator (commit `a6fba65`).
Day 4 per the sprint plan (`~/workspace/your_files/erpcraft-hackathon-sprint-plan.md`):
**MCP server + CLI: every command exposed. JSON control channel.**
Exit criterion: an agent (or curl) can drive the full loop without the UI.

Planning surface this reconciles against:
`~/workspace/your_files/erpcraft-agent-contract-spec.md` — its MCP/CLI
command list was written Oct 13, before any code existed. The names and
shapes below follow the **core as built**, not the spec, wherever they
differ; the spec's intent table and parsing rules are unchanged and bind
Day 5, not Day 4.

House rules (`AGENTS.md`) hold throughout: money is integer cents, the DB
triggers are the enforcer, the journal is append-only, agents propose /
humans confirm / then it posts, tests accompany every command. **Cut
agent polish or UI before touching a ledger invariant.**

## The one architectural rule (the `bt mcp` precedent)

One dispatch table, three doors. Day 4 adds a third workspace member,
`crates/erpcraft-cli`, exposing:

- `erpcraft <command> [args]` — human/operator CLI
- `erpcraft mcp` — MCP server on stdio (same pattern as `bt mcp` in the
  rust-backtester: tools route into the same functions as the CLI, no
  second implementation)

Both doors call `erpcraft-core` functions directly — never HTTP to the
server, never re-implemented posting logic. The HTTP server stays as-is;
Day 4 does not refactor it onto the dispatch table (that's a post-sprint
cleanup, noted here so nobody tries it mid-sprint).

CLI contract (the fleet's one CLI contract): **JSON on stdout, errors on
stderr in plain language, exit 0 = ok / 1 = refused or invalid /
2 = internal error.** LedgerError messages pass through verbatim — they
are already written for non-accountants.

DB selection mirrors the server: `ERPCRAFT_DB` env, same default path
handling as `erpcraft-server` (`open_db` + `init_schema` +
`seed_if_empty` + `seed_master_data_if_empty` +
`seed_configurator_if_empty` on open, exactly as the server boots).

## Command map — spec name → core as built → Day 4 action

Order-to-cash (all exist in core; all but one already have HTTP routes):

| Spec (Oct 13) | Core as built | Day 4 tool / command |
|---|---|---|
| `create_quote(customer, lines)` | `create_quote(&mut conn, &NewQuote) -> i64` — lines carry optional configuration + snapshot pricing | `create_quote` |
| — (spec had no quote lifecycle) | `send_quote` / `accept_quote` / `decline_quote` / `expire_quote(id)` | `send_quote`, `accept_quote`, `decline_quote`, `expire_quote` |
| `confirm_order(quote_id)` | `create_order_from_quote(&mut conn, quote_id) -> i64` | `create_order` (arg: quote_id) |
| — | `fulfil_order(id)`, `cancel_order(id)` | `fulfil_order`, `cancel_order` |
| `ship_and_invoice(order_id)` | **Two steps as built, not one:** `create_invoice_from_order(&mut conn, order_id) -> i64`, then `issue_invoice(&mut conn, invoice_id)` — issuance is where revenue + COGS post (Day 3, from the line snapshots) | `create_invoice`, `issue_invoice` — do NOT collapse into one tool; Day 5's agent needs the draft invoice visible before it posts |
| `record_payment(customer, amount, invoice_ref?)` | `record_payment(&mut conn, invoice_id, amount_cents) -> i64` — keyed by invoice, not customer; overpayment is refused in core | `record_payment` (args: invoice_id, amount_cents) |
| `record_return(customer, units, original_sale_ref)` | `record_sales_return(&mut conn, invoice_id, &[ReturnLine]) -> (i64, i64)` (the two reversing entry ids) | `record_return` (args: invoice_id, lines[{item_id, qty}]) |
| `trial_balance()` | `trial_balance(&conn) -> TrialBalance` | `trial_balance` |
| `account_ledger(account)` | `account_ledger(&conn, code) -> AccountLedger` | `account_ledger` |
| `audit_trail(posting_ref)` | `get_entry(id)`, `list_entries()` — full who/what/when/why is Day 6; Day 4 exposes the reads only | `get_entry`, `list_entries` |

Reads (no postings, no gate): `list_customers`, `list_items`,
`list_quotes`, `get_quote`, `list_orders`, `get_order`, `list_invoices`,
`get_invoice`, `list_payments`, `list_payments_for_invoice`, plus a
`health` command returning the same counts as `GET /health`.

Configurator (read/evaluate door for Day 4; authoring stays HTTP/CLI-stretch):

- `list_config_models`, `get_config_model` — reads
- `evaluate_configuration(conn, model_id, &BTreeMap<String, ConfigValue>)`
  → tool `evaluate_config` (args: model_id, configuration map). Returns
  the full `Evaluation`: verdict, forced values/components, and the
  structured `Explanation`s — the AI-visualization surface Mark named
  as the epic's strength starts here, as data, not prose.
- Stretch, only if the core loop is green first: `create_config_model`,
  `add_characteristic`, `create_component`, `create_dependency`.

## The returns gap — close it on Day 4, don't inherit it

`record_sales_return` shipped in the Day-3 core and is covered by core
tests, but it has **no HTTP route** in `erpcraft-server/src/main.rs`
(verified Oct 28: no handler, no route). The full loop therefore cannot
be driven end-to-end over HTTP today. Day 4's CLI/MCP calls the core
function directly, which closes the gap for the agent surface. Adding
the missing `POST /invoices/{id}/returns` HTTP route is a five-line
route + handler — do it on Day 4 only if it's free; otherwise log it
here as a known HTTP-surface gap for Day 7 hardening. Do not let the
MCP surface skip returns because HTTP did.

## Manual journal intents (agent-contract intents 3–6, 8, 10–12)

The spec's named commands `buy_materials`, `pay_supplier`,
`issue_materials`, `record_labor`, `record_deposit`, `record_rent`,
`record_interest`, `record_depreciation` have **no dedicated core
functions** — on the oracle day they are plain `post_entry` calls with
the spec's account pairs (1200/2000, 2000/1000, 1210/1200, 1210/1000,
1000/2200, 6000/1000, 6010/1000, 6020/1310). Day 4 exposes them as
**thin builders in the CLI/MCP layer only**: each builder constructs a
`NewEntry` (two lines, integer cents, memo from the command name) and
calls the one `post_entry`. No builder writes SQL, computes a balance,
or bypasses `post_entry_in`. A generic `post_entry` command (raw
`NewEntry` JSON in) is also exposed — it's the CLI equivalent of
`POST /journal-entries` and the fallback for anything unnamed.

## Provenance: `source` is already plumbed — use it

`NewEntry.source` exists today ("manual" | "agent" | "opening" |
"correction") and lands on every journal entry. Day 4 sets it at the
door, never in core: CLI builds use `source: "cli"` and MCP builds use
`source: "mcp"` — two new values beyond the documented set, so extend
the doc comment on `NewEntry.source` in the same commit, and keep
Day 5's agent on `"agent"`. Day 6's audit trail (who/what/when/why)
depends on this being right from the first MCP-posted entry, not
backfilled.

## The confirmation gate on Day 4 — explicitly out of the tools

The propose → confirm → post gate is the **Day 5 agent layer's**
contract, not a property of the Day 4 tools. Day 4's CLI/MCP are
operator doors: the human at the terminal (or the MCP client acting on
a confirmed proposal) is the confirmer. Do NOT build a confirm flag,
a dry-run mode, or a two-step token into Day 4 tools to simulate the
gate — Day 5 implements propose/confirm above this surface, and a
half-gate in the tools would give two places for it to be wrong.
One exception: every posting tool returns the full posted entry
(read back via `get_entry`), so whatever drives it can show the human
exactly what posted — the same read-back the HTTP handlers do.

## Exit tests for Day 4 (all must pass before Day 5 starts)

1. **Full loop, no UI, no HTTP:** via MCP calls alone (stdio test
   harness) — seed → quote (configured BRK-100 line included) → send →
   accept → order → fulfil → invoice → issue → payment → return —
   ending trial balance matches the equivalent core-level run.
2. **Oracle replay via CLI:** the oracle day driven through the CLI
   lands on $129,975.00 per side, per account — same assertion as the
   Day-3 suite, new door.
3. **Configured COGS trap, again:** a configured steel+galvanized
   invoice issued via MCP posts COGS from the $91.00 snapshot, not
   $68.00 (the Day-3 trap, re-proven through the agent door).
4. **Refusals are plain and post nothing:** overpayment, issue of a
   draft-state violation, unknown account in `post_entry`, and an
   aluminum+galvanized evaluation refusal (structured explanations
   intact through JSON) — each exits 1, stderr carries the core
   message, entry count unchanged.
5. **Parity of doors:** the same command sequence via CLI and via MCP
   produces identical trial balances and identical entry memos/sources
   except the `source` tag itself (`cli` vs `mcp`).
6. Workspace suite green (`cargo test`), clippy clean — the standing
   bar, no exceptions.

## Not Day 4 (parked, on purpose)

- Natural-language parsing, propose/confirm flow — Day 5.
- Audit trail UI/API, prompt provenance beyond `source` — Day 6.
- HTTP returns route (see gap above) and HTTP refactor onto the
  dispatch table — Day 7 hardening or post-sprint.
- Batch confirmations / risk tiers — already in ROADMAP.md.
