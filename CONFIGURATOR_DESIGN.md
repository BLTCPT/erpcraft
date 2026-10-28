# ERPCraft — Variant Configurator design (parked epic)

Status: **design + build.** Written Tue Oct 27
evening at Mark's direction ("integrating the variant configurator…
something like SAP uses: a precondition, condition, selector,
constraint"), following the sprint rule that new ideas go to the
ROADMAP, not the build. Day 3's GL posting (Wed Oct 28, $129,975.00
oracle) and Day 5's NL agent (Fri Oct 30) remain the submission spine.
Mark's decisions, Tue Oct 27 evening: (1) **do the hard work now** —
the build starts immediately rather than waiting for the original
spike gate, and variant pricing rolls selected components into the
line price in v1 (see "Pricing"); (2) object dependencies become
difficult for humans to visualize as product models grow — AI
visualization / explanation of the dependency network is a named
strength of this epic, not an afterthought (see "AI at the edges").

## Why this exists

Configurable products are where supply chains actually get hard: one
sellable bracket is really a family of variants, and the variant —
not the catalog item — decides which components get bought, built,
and shipped. Scale that from a bracket to configure-to-order hardware
at an Nvidia-class catalog — thousands of valid variants, each
exploding into its own component and supply commitments — and the
configurator stops being a sales convenience and becomes the thing
the supply chain is planned from. SAP LO-VC's dependency model is
the proven shape for
this and also the miserable part (stringly-typed dependency source,
opaque failures). ERPCraft steals the model and fixes the two
implementation sins: dependencies are **typed data, never source
text**, and every rejection carries a **structured explanation** as a
first-class output.

## The dependency model (SAP LO-VC → ERPCraft)

Four dependency kinds, stored as typed expression trees (below),
evaluated by a deterministic evaluator in `erpcraft-core`:

| Kind | SAP role | ERPCraft semantics |
|---|---|---|
| **Precondition** | Gates which characteristics / values are in play | Evaluated first. A characteristic (or value) whose precondition is false is removed from the selectable space before anything else runs — it cannot be chosen, forced, or validated against. |
| **Selection condition** | Decides whether a value / component is selected or required | When true, the target value or component **is** selected. This is the supply-chain hook: selection conditions are what turn a configuration into a concrete component list at order time. |
| **Constraint** | Cross-characteristic consistency | When violated, the configuration is **inconsistent** and the evaluator reports exactly which constraint fired on which values. Constraints never silently repair a configuration. |
| **Procedure / derivation** *(optional, later)* | Derives a value from other values | Out of spike scope. If added, derivations are pure functions over the same typed AST and are recorded in the explanation trace like any other firing. |

"The selector" is not a fifth stored thing — it is the evaluator's
job: given a partial configuration, return per characteristic the
values still selectable, the values forced by selection conditions,
and a consistent / inconsistent / incomplete verdict.

## Data model (proposed — no schema built yet)

Same house rules as the rest of the core: integer cents for any money,
invariants enforced in the core, snapshots on documents.

- `config_models (id, item_id FK, name)` — a configurable item (the
  demo: `BRK-100`, already seeded at $125.00 / $68.00 standard cost)
  has at most one active model.
- `characteristics (id, model_id FK, code, name, value_type)` —
  `value_type ∈ {enum, integer, bool}`; money and free text are
  deliberately not characteristic types in v1.
- `characteristic_values (id, characteristic_id FK, code, label)` —
  the enum domain; integer characteristics carry min/max instead.
- `dependencies (id, model_id FK, kind, target, expr, source_text,
  created_by)` — `kind ∈ {precondition, selection, constraint}`;
  `target` names the characteristic, value, or component the
  dependency governs; `expr` is the typed AST (below), stored as
  JSON; `source_text` keeps the human sentence it was authored from
  for the audit trail — it is provenance, never parsed.
- `components (id, model_id FK, component_item_id FK, qty,
  price_delta_cents, cost_delta_cents)` — the
  candidate component list a configuration explodes into; which
  components survive is decided by selection conditions, not by
  storing per-variant BOMs. The deltas drive line-price roll-up
  (see "Pricing").
- **Configuration snapshot on the quote line:**
  `quote_lines` gains a nullable `configuration` (JSON: characteristic
  code → value code, plus the model/dependency version evaluated
  against). Snapshotted at quote time exactly the way Day 2 already
  snapshots `unit_price_cents` — later dependency edits never rewrite
  a quoted configuration. Orders copy the snapshot; invoices never
  re-evaluate.

## Expression language — typed AST, no strings

One small recursive enum, serialised as JSON, constructed by code or
by the agent's authoring flow — never by parsing LO-VC-style syntax:

```
Expr = Eq(char, value) | In(char, [value…]) | Cmp(char, op, int)
     | And([Expr…]) | Or([Expr…]) | Not(Expr)
```

Deliberately inexpressive: no arithmetic beyond integer comparison,
no references to documents, money, or inventory. If a real dependency
cannot be said in this language, that is a design discussion, not a
reason to grow a scripting language inside the ERP.

## Evaluator contract (the core of the design)

`evaluate(model, partial_config) -> Evaluation`, a **pure,
deterministic function** in the core — same inputs, same output, no
DB writes, no AI inside:

1. **Prune** — apply preconditions; drop out-of-play characteristics
   and values. A chosen value whose precondition fails is a conflict,
   reported, not silently dropped.
2. **Select** — apply selection conditions to a fixpoint (a selection
   can enable another precondition). Cycles are an error at
   dependency-save time (checked then), never an infinite loop at
   evaluation time.
3. **Check** — evaluate every constraint against the resulting
   configuration.

`Evaluation` returns, every time:

- `selectable: char → [values still allowed]`
- `forced: char → value + the dependency that forced it`
- `verdict: Consistent | Incomplete(missing required chars) |
  Inconsistent`
- `explanations: [{ dependency_id, kind, message_parts }]` —
  structured (dependency, characteristics, values involved), so the
  server, CLI, MCP, and agent all render the *same* reason in their
  own words. A rejection without an explanation is a bug.

## Where it plugs into order-to-cash

- **Quote:** a configurable line must evaluate to `Consistent`
  (incomplete is allowed while drafting a quote, never at
  `sent → accepted`). The snapshot freezes configuration + prices.
- **Order (from an accepted quote, existing Day-2 rule):** the
  snapshot explodes via selection conditions into component lines —
  this list is what a future procurement/manufacturing epic consumes.
  Order-to-cash underneath proceeds **unchanged**.
- **Invoice / GL:** untouched. Variants do not post; documents post
  (Day 3 map). Variant price/cost adjustments land as explicit line
  prices at quote time — visible in the snapshot, never as evaluator
  side effects (see "Pricing").

## Pricing — selected components roll into the line price (Mark's call)

Flat item price for configured lines was the easy option; Mark chose
the hard one on Oct 27: **a configured quote line's price is computed
from what the configuration actually selects.**

- Each component carries a `price_delta_cents` and a `cost_delta_cents`
  (either may be zero; deltas may be negative for a lighter variant).
- At quote time, once evaluation reaches a fixpoint, the line's
  `unit_price_cents` = item base price + Σ price deltas of the selected
  components, and the line's standard cost likewise = item standard
  cost + Σ cost deltas. Both are computed in the core, in integer
  cents, and **snapshotted on the quote line** alongside the
  configuration — the evaluator itself stays pure and never touches
  prices; the pricing step consumes its `forced`/selected output.
- Unconfigured lines (no configuration present) price exactly as
  Day 2 priced them — the Day-3 oracle arithmetic for plain BRK-100
  lines ($125.00 / $68.00) is unchanged.
- A computed line price or cost that goes negative is a constraint
  violation, reported through the same explanation channel — never
  clamped silently.

## AI at the edges — never inside evaluation

Same propose → confirm gate as journal postings (AGENTS.md rule 4):

- **Authoring:** "orders over 40 units need the reinforced mount" →
  the agent drafts a typed constraint (AST + the sentence it came
  from) → a human confirms → only then is the dependency saved.
  The agent never saves a dependency directly.
- **Explanation:** when evaluation rejects a configuration, the agent
  may render the structured explanation in plain language. It may
  not invent reasons: every claim in the explanation must trace to a
  fired `dependency_id` in the `Evaluation`.
- **Visualization:** as product models grow, the object-dependency
  network outgrows what a human can hold in their head — this is the
  AI's clearest strength in the epic (Mark, Oct 27). The agent
  renders the dependency graph for a model or a single
  characteristic — what it depends on, what depends on it, which
  constraints bind it — always from the stored typed dependencies,
  never from a paraphrase of them.
- **Never:** the agent does not evaluate, override, or repair a
  configuration. A configurator that hallucinates constraints is
  worse than SAP's.

## Demo configuration (design sketch — not frozen data)

`BRK-100` as the configurable bracket: characteristics `material`
(steel / aluminum), `finish` (bare / powder-coat / galvanized),
`mounting` (standard / reinforced), `quantity` (integer, from the
quote line). Illustrative dependencies:

- Precondition: `galvanized` finish is in play only for `steel`.
- Selection: `mounting = reinforced` selects the reinforced-mount
  component; `material = aluminum` selects the aluminum blank
  component instead of the steel blank.
- Constraint: `quantity > 40` requires `mounting = reinforced` —
  the exact sentence used in the authoring example above.

These examples are illustrative until Mark freezes a demo set; they
do not change the seeded `BRK-100` price/cost or any Day-3 number.

## Build scope and exit tests

Core build, started Oct 27 evening (ahead of the original Day 3 +
Day 5 gate, per Mark): characteristics + the three dependency kinds +
`evaluate` + one configured quote line validated end to end +
component roll-up pricing (above). Exit tests:

1. Precondition pruning removes a value and rejects it if chosen.
2. Selection condition forces a value/component and names itself in
   `forced`.
3. Constraint violation → `Inconsistent` with the firing dependency
   and values in `explanations`.
4. Determinism: same model + same partial config → byte-identical
   `Evaluation`.
5. Snapshot immutability: editing a dependency after quoting does not
   change the quote's stored configuration or its evaluation record.
6. No-GL guard, configurator edition: evaluating a configuration
   creates zero journal entries and zero documents.
7. Pricing roll-up: a configured line's snapshotted price/cost equal
   base + Σ selected component deltas; unselected components
   contribute nothing; an unconfigured line prices exactly as Day 2.

If the build misses, the design doc alone is the submission asset —
it is the differentiator in the writeup either way.

## Open questions for Mark (none blocking the build)

- Demo characteristic set for BRK-100: the sketch above is the build
  default until Mark freezes his own from real SAP configuration
  work.
- ~~Variant pricing in v1~~ — answered Oct 27: selected components
  roll into the line price (see "Pricing").
