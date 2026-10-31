# MRP_DESIGN.md — AI-native MRP, Warp Speed pattern

ERPCraft's materials planning, modeled on the *pattern* of Palantir
Warp Speed (Palantir's manufacturing OS): planning that re-computes
continuously from current operational state instead of a nightly
batch, propagates changes the moment they land, and anticipates
shortages with their reasons attached. No Palantir product, API, or
data is involved — this is the design pattern, implemented in
ERPCraft's deterministic core like everything else.

## The three properties

1. **Dynamic.** A plan is never stored. `run_mrp` computes from the
   state that exists *at the moment of the run* — open sales orders,
   stock levels, WIP, open supply, BOMs, and the configurator's
   current component structure. Change an order or a configuration
   and the next run reflects it; a test proves exactly that.
2. **Explainable.** Every planned order carries its netting math
   (gross requirement, on hand, on order, WIP, net) and the demand
   reasons behind it (which order or what-if line drove how much,
   what stock covered, what the plan must cover). A shortage is a
   planned order whose start date is already past, with `days_late`
   and the same reasons — never a bare red flag.
3. **Agent-native.** MRP is dispatch commands and MCP tools
   (`run_mrp`, `list_planned_orders`, `explain_shortage`,
   `release_planned_order`, …), and the NL agent answers planning
   questions from MRP output ("Can we build 200 BRK-100 by Nov 15?",
   "What are we short on?") and can *propose* releasing a planned
   order behind the same propose→confirm gate as every posting.
   Releasing creates the production/purchase document; **no GL posts
   on release** — value moves only through the existing posting map
   (issue materials / labor / complete production builders, supplier
   payments) when the real event happens.

## Data model (core, `mrp.rs`)

- `items.lead_time_days`, `items.make_or_buy` — planning attributes
  (migration columns, like the Day-3 guards).
- `bom_lines(parent_item_id, component_item_id, qty_per)` — the
  stocked BOM for unconfigured production.
- `stock_levels(item_id, on_hand, on_order, wip)` — quantities.
  The GL holds inventory *value*; MRP holds *quantities*; the two
  meet only through the existing posting map.
- `production_orders` / `purchase_orders` — released documents
  (status open → completed/received/cancelled). Releasing a planned
  order is the only way they are created; receiving/completing one
  moves `on_order`/`wip` into `on_hand`.

**Explosion rule.** A demand line with a configuration snapshot
explodes through the snapshot's selected components that are linked
to stock items (the configurator's component structure), *plus* BOM
lines whose component is not configurator-managed (fasteners are
always fasteners). A line without a configuration is the standard
build and explodes through the BOM alone. Configurator-managed
items are never double-counted.

**Netting.** Per item, requirement lines are covered by available
stock (on hand + on order + WIP) in need-date order; the uncovered
remainder is the planned order (lot-for-lot). Component requirements
are generated only from the planned remainder, dated at the parent's
start date (due date − lead time). Items are processed in BOM level
order, so a plan is one deterministic pass — same state, same plan,
byte for byte.

## Demo seed

Planning data is demo *scenario* data, seeded by the doors
(`seed_mrp_demo_if_empty`, called on CLI/MCP/HTTP boot) rather than
by `open_db`: the core's frozen master-data contract (one seeded
item) is a Day-2 test surface and does not change. The seed gives
BRK-100 a BOM (steel blank ×1, fastener set ×1), links the
configurator's blank/mount components to stock items, sets lead
times (BRK-100 make 5d; steel blank buy 7d; aluminum blank 10d;
mount kit 4d; fasteners 3d) and believable stock (40 finished
brackets + 10 in WIP, 120 steel blanks + 50 on order, …).

## Honest limits

Single-level date offsetting per BOM level; lot-for-lot only; no
safety stock, no capacity model, no supplier masters (a purchase
order names an item, quantity, and due date — procurement depth is
still ROADMAP scope). Component stock is consumed implicitly by the
plan, not decremented at completion; `set_stock` exists so an
operator (or the demo) can record the physical truth.
