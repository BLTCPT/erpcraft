//! The one dispatch table behind both Day-4 doors (docs/DAY4_PREP.md).
//!
//! Every command — CLI subcommand and MCP tool alike — resolves to one
//! function here, and every function calls `erpcraft-core` directly.
//! There is no second implementation of anything: the CLI in `main.rs`
//! and the MCP shell in `mcp.rs` only translate their transports into
//! `(command name, JSON arguments)` and render the JSON result (or the
//! plain-language refusal) back out.
//!
//! Contract (the fleet's one CLI contract): results are JSON values;
//! refusals carry the core's `LedgerError` message verbatim — they are
//! already written for non-accountants. The doors map a refusal to
//! exit 1 / MCP `isError`, and an internal failure to exit 2.
//!
//! Provenance: journal entries posted through this layer (the manual
//! journal builders and the raw `post_entry` command) are tagged at
//! the door — `source: "cli"` or `source: "mcp"` — never in core.
//! Engine postings (issuance, payments, returns) keep the core's own
//! `invoice:<id>` / `payment:<id>` / `return:<id>` sources on both
//! doors, exactly as over HTTP.

use erpcraft_core::rusqlite::{ffi::ErrorCode, Connection, Error as SqliteError};
use erpcraft_core::{
    accept_quote, account_count, account_ledger, add_bom_line, approve_requisition, audit_entry,
    audit_trail, cancel_order, cancel_purchase_order, cancel_requisition, close_fiscal_period,
    complete_production_order, config_model_count, create_fiscal_period, create_invoice_from_order,
    create_order_from_quote, create_purchase_order, create_quote, create_requisition,
    create_supplier, create_user, customer_count, decline_quote, entry_count,
    evaluate_configuration, expire_quote, fulfil_order, get_ap_invoice, get_config_model,
    get_entry, get_invoice, get_order, get_procurement_order, get_quote, get_requisition,
    invoice_count, issue_invoice, list_ap_invoices, list_ap_payments, list_ap_payments_for_invoice,
    list_bom, list_config_models, list_control_settings, list_customers, list_entries,
    list_fiscal_periods, list_goods_receipts, list_invoices, list_items, list_orders,
    list_payments, list_payments_for_invoice, list_procurement_orders, list_production_orders,
    list_purchase_orders, list_quotes, list_requisitions, list_stock, list_suppliers, list_users,
    open_db, order_count, override_ap_match, pay_ap_invoice_attributed, payment_count, post_entry,
    quote_count, receive_goods, receive_purchase_order, record_ap_invoice,
    record_payment_attributed, record_sales_return, reject_requisition, release_planned_order,
    run_mrp, send_quote, set_approval_threshold, set_stock, trial_balance, ApLineInput,
    AuditFilter, ConfigValue, Evaluation, Explanation, LedgerError, MrpDemand, NewApInvoice,
    NewEntry, NewProcurementOrder, NewQuote, NewRequisition, ReceiveLineInput, ReturnLine,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Which door a command arrived through. Only used to tag the journal
/// `source` of entries this layer posts (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Door {
    Cli,
    Mcp,
    /// The Day-5 natural-language agent (see `crate::agent`). Its
    /// postings carry `source: "agent"` — the audit decision in
    /// docs/DAY5_PREP.md: the tag where the door controls it, the verbatim
    /// prompt in the memo (agent-supplied builder memos are honored
    /// only through this door).
    Agent,
}

impl Door {
    pub fn source_tag(self) -> &'static str {
        match self {
            Door::Cli => "cli",
            Door::Mcp => "mcp",
            Door::Agent => "agent",
        }
    }
}

/// A command failure, in the shape both doors render.
///
/// * `message` — plain language, passed to the user verbatim.
/// * `explanations` — the configurator's structured reasons, when the
///   refusal came from the evaluator (never dropped at the door).
/// * `evaluation` — the full evaluation, attached when the refusal is
///   an inconsistent `evaluate_config` verdict.
/// * `internal` — true when the failure is ours (database I/O and the
///   like), false when the command was refused or invalid. The CLI
///   maps this to exit 2 vs exit 1.
#[derive(Debug)]
pub struct DispatchError {
    pub message: String,
    pub explanations: Vec<Explanation>,
    pub evaluation: Option<Box<Evaluation>>,
    pub internal: bool,
}

impl DispatchError {
    pub fn refused(message: impl Into<String>) -> Self {
        DispatchError {
            message: message.into(),
            explanations: Vec::new(),
            evaluation: None,
            internal: false,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        DispatchError {
            message: message.into(),
            explanations: Vec::new(),
            evaluation: None,
            internal: true,
        }
    }

    /// The MCP error payload: the message, plus the structured
    /// explanations / evaluation when the refusal carries them.
    pub fn to_json(&self) -> Value {
        let mut out = json!({ "error": self.message });
        if !self.explanations.is_empty() {
            out["explanations"] = serde_json::to_value(&self.explanations).unwrap_or(Value::Null);
        }
        if let Some(evaluation) = &self.evaluation {
            out["evaluation"] = serde_json::to_value(evaluation).unwrap_or(Value::Null);
        }
        out
    }
}

impl From<LedgerError> for DispatchError {
    fn from(e: LedgerError) -> Self {
        match e {
            LedgerError::Configuration {
                message,
                explanations,
            } => DispatchError {
                message,
                explanations,
                evaluation: None,
                internal: false,
            },
            LedgerError::Db(db) => {
                // A database *refusal* — the balance trigger, a CHECK
                // or a foreign key firing — is the caller's entry being
                // rejected, not an internal failure; the HTTP door
                // answers it 400 for the same reason. Genuine database
                // trouble (I/O, corruption) stays internal.
                let refused_by_the_db = matches!(
                    &db,
                    SqliteError::SqliteFailure(f, _)
                        if f.code == ErrorCode::ConstraintViolation
                );
                let message = LedgerError::Db(db).to_string();
                if refused_by_the_db {
                    DispatchError::refused(message)
                } else {
                    DispatchError::internal(message)
                }
            }
            other => DispatchError::refused(other.to_string()),
        }
    }
}

pub type DispatchResult = Result<Value, DispatchError>;

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

/// One command in the dispatch table: its name (identical as a CLI
/// subcommand and as an MCP tool), what it does, its CLI usage line,
/// the argument a bare positional value maps to, and its MCP input
/// schema. The CLI help and the MCP `tools/list` are both generated
/// from this table — there is nothing else to drift.
pub struct CommandSpec {
    pub name: &'static str,
    pub summary: &'static str,
    pub usage: &'static str,
    pub primary_arg: Option<&'static str>,
    pub input_schema: Value,
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

fn id_prop(description: &str) -> Value {
    json!({ "type": "integer", "description": description })
}

/// The shared planning-run arguments: an as-of date plus optional
/// what-if demand (one line). Used by every MRP read/release command
/// so a plan is always recomputed from identical inputs.
fn mrp_args_schema(extra: &[(&str, Value)], required: &[&str]) -> Value {
    let mut props = json!({
        "as_of": { "type": "string", "description": "Plan as of this date, YYYY-MM-DD (default: today)" },
        "demand_sku": { "type": "string", "description": "What-if demand: item SKU" },
        "demand_qty": { "type": "integer", "minimum": 1, "description": "What-if demand: quantity" },
        "demand_date": { "type": "string", "description": "What-if demand: needed-by date, YYYY-MM-DD" },
        "demand_config": {
            "type": "object",
            "description": "What-if demand: configuration values (characteristic code → value) for a configured item",
            "additionalProperties": true
        }
    });
    for (name, prop) in extra {
        props[name] = prop.clone();
    }
    schema(props, required)
}

fn req_str(args: &Value, key: &str) -> Result<String, DispatchError> {
    opt_str(args, key)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            DispatchError::refused(format!("argument '{key}' is required and must be a string"))
        })
}

fn opt_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| v.as_i64())
}

fn mrp_demand_from_args(args: &Value) -> Result<Vec<MrpDemand>, DispatchError> {
    let mut demand = Vec::new();
    if let Some(sku) = opt_str(args, "demand_sku") {
        let qty = req_i64(args, "demand_qty").map_err(|_| {
            DispatchError::refused(
                "what-if demand needs demand_qty and demand_date alongside demand_sku".to_string(),
            )
        })?;
        let date = req_str(args, "demand_date").map_err(|_| {
            DispatchError::refused(
                "what-if demand needs demand_qty and demand_date alongside demand_sku".to_string(),
            )
        })?;
        let configuration = match args.get("demand_config") {
            Some(v) if !v.is_null() => Some(
                serde_json::from_value(v.clone())
                    .map_err(|e| DispatchError::refused(format!("invalid demand_config: {e}")))?,
            ),
            _ => None,
        };
        demand.push(MrpDemand {
            sku: sku.to_string(),
            qty,
            need_date: date,
            configuration,
        });
    }
    Ok(demand)
}

fn mrp_as_of(args: &Value) -> String {
    opt_str(args, "as_of")
        .map(|s| s.to_string())
        .unwrap_or_else(today_utc)
}

pub fn specs() -> Vec<CommandSpec> {
    let no_args = schema(json!({}), &[]);
    vec![
        // -- Reads ---------------------------------------------------
        CommandSpec {
            name: "health",
            summary: "Health check: the same counts as GET /health (accounts, journal entries, customers, quotes, orders, invoices, payments, config models).",
            usage: "erpcraft health",
            primary_arg: None,
            input_schema: no_args.clone(),
        },
        CommandSpec {
            name: "trial_balance",
            summary: "Trial balance over posted entries: every account on its debit or credit side, with totals.",
            usage: "erpcraft trial_balance",
            primary_arg: None,
            input_schema: no_args.clone(),
        },
        CommandSpec {
            name: "account_ledger",
            summary: "One account's posted journal lines in entry order, with a running balance after each line.",
            usage: "erpcraft account_ledger <code>",
            primary_arg: Some("code"),
            input_schema: schema(
                json!({ "code": { "type": "string", "description": "Account code, e.g. 1100" } }),
                &["code"],
            ),
        },
        CommandSpec {
            name: "get_entry",
            summary: "One journal entry with its lines, by id.",
            usage: "erpcraft get_entry <entry_id>",
            primary_arg: Some("entry_id"),
            input_schema: schema(json!({ "entry_id": id_prop("Journal entry id") }), &["entry_id"]),
        },
        CommandSpec {
            name: "list_entries",
            summary: "All journal entries, oldest first, with their lines.",
            usage: "erpcraft list_entries",
            primary_arg: None,
            input_schema: no_args.clone(),
        },
        CommandSpec {
            name: "audit_entry",
            summary: "One journal entry fully explained (Day 6 audit trail): who posted it, the document it belongs to, why it posted, the verbatim prompt for agent entries, and the full provenance chain (quote → order → invoice → payments → returns) with reversal links both ways.",
            usage: "erpcraft audit_entry <entry_id>",
            primary_arg: Some("entry_id"),
            input_schema: schema(json!({ "entry_id": id_prop("Journal entry id") }), &["entry_id"]),
        },
        CommandSpec {
            name: "audit_trail",
            summary: "The audit trail (Day 6): every posted entry fully explained, oldest first. Optional filters: source (exact tag, e.g. agent / cli / invoice:3), document (e.g. invoice:3, payment:2, sales_return:3), from_date / to_date (inclusive, YYYY-MM-DD).",
            usage: "erpcraft audit_trail [--source agent] [--document invoice:3] [--from-date 2026-10-01] [--to-date 2026-10-31]",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "source": { "type": "string", "description": "Exact source tag filter" },
                    "document": { "type": "string", "description": "Document filter, e.g. invoice:3" },
                    "from_date": { "type": "string", "description": "Earliest entry date (inclusive)" },
                    "to_date": { "type": "string", "description": "Latest entry date (inclusive)" }
                }),
                &[],
            ),
        },
        // ---- MRP (MRP_DESIGN.md): dynamic, explainable planning ---------
        CommandSpec {
            name: "run_mrp",
            summary: "Run MRP (dynamic — computed from current state at call time, never a stored batch): nets open sales orders plus optional what-if demand (demand_sku/demand_qty/demand_date/demand_config) against on-hand + on-order + WIP, explodes through BOMs and configurator component structure, and returns per-item netting with reasons, planned production/purchase orders, and the shortage list (planned orders whose start date is already past).",
            usage: "erpcraft run_mrp [--as-of 2026-10-31] [--demand-sku BRK-100 --demand-qty 200 --demand-date 2026-11-15]",
            primary_arg: None,
            input_schema: mrp_args_schema(&[], &[]),
        },
        CommandSpec {
            name: "list_planned_orders",
            summary: "The planned orders + shortages from a fresh MRP run (same optional as-of / what-if demand arguments as run_mrp). Each planned order has a key (kind:sku:due_date:qty) that release_planned_order accepts.",
            usage: "erpcraft list_planned_orders [--as-of 2026-10-31] [--demand-sku BRK-100 --demand-qty 200 --demand-date 2026-11-15]",
            primary_arg: None,
            input_schema: mrp_args_schema(&[], &[]),
        },
        CommandSpec {
            name: "explain_shortage",
            summary: "Explain one item's shortage position in a fresh MRP run: whether it is short, by how much, when the planned order should have started, and the demand reasons + netting math behind it.",
            usage: "erpcraft explain_shortage --sku BRK-100 [--as-of 2026-10-31]",
            primary_arg: Some("sku"),
            input_schema: mrp_args_schema(
                &[("sku", json!({ "type": "string", "description": "Item SKU to explain" }))],
                &["sku"],
            ),
        },
        CommandSpec {
            name: "release_planned_order",
            summary: "Release one planned order from a fresh MRP run (pass the same as-of / what-if demand arguments that produced its key): creates the production order (make items) or purchase order (buy items) and counts it as WIP / on-order supply. Posts NO journal entry — value moves only through the existing builders when real events happen.",
            usage: "erpcraft release_planned_order --key purchase:FST-SET:2026-11-10:60 [--as-of 2026-10-31] [--demand-sku BRK-100 --demand-qty 200 --demand-date 2026-11-15]",
            primary_arg: Some("key"),
            input_schema: mrp_args_schema(
                &[("key", json!({ "type": "string", "description": "Planned-order key from list_planned_orders / run_mrp" }))],
                &["key"],
            ),
        },
        CommandSpec {
            name: "list_stock",
            summary: "Quantity state for MRP: every item with its make-or-buy flag, lead time, on-hand, on-order, and WIP quantities.",
            usage: "erpcraft list_stock",
            primary_arg: None,
            input_schema: no_args.clone(),
        },
        CommandSpec {
            name: "set_stock",
            summary: "Record the physical quantity truth for one item (on_hand / on_order / wip). Quantities only — inventory value lives in the GL and moves through the posting builders.",
            usage: "erpcraft set_stock <item_id> --on-hand 120 [--on-order 50] [--wip 0]",
            primary_arg: Some("item_id"),
            input_schema: schema(
                json!({
                    "item_id": id_prop("Item id"),
                    "on_hand": { "type": "integer", "minimum": 0, "description": "Units on hand" },
                    "on_order": { "type": "integer", "minimum": 0, "description": "Units on open purchase orders" },
                    "wip": { "type": "integer", "minimum": 0, "description": "Units in work in progress" }
                }),
                &["item_id", "on_hand"],
            ),
        },
        CommandSpec {
            name: "list_bom",
            summary: "The stocked bill of materials for one parent item.",
            usage: "erpcraft list_bom <parent_item_id>",
            primary_arg: Some("parent_item_id"),
            input_schema: schema(json!({ "parent_item_id": id_prop("Parent item id") }), &["parent_item_id"]),
        },
        CommandSpec {
            name: "add_bom_line",
            summary: "Add a component to an item's stocked bill of materials (qty per parent unit).",
            usage: "erpcraft add_bom_line --parent-item-id 1 --component-item-id 2 --qty-per 1",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "parent_item_id": id_prop("Parent item id"),
                    "component_item_id": id_prop("Component item id"),
                    "qty_per": { "type": "integer", "minimum": 1, "description": "Component units per parent unit" }
                }),
                &["parent_item_id", "component_item_id", "qty_per"],
            ),
        },
        CommandSpec {
            name: "list_production_orders",
            summary: "Released production orders (documents created by release_planned_order), oldest first.",
            usage: "erpcraft list_production_orders",
            primary_arg: None,
            input_schema: no_args.clone(),
        },
        CommandSpec {
            name: "list_purchase_orders",
            summary: "Released purchase orders (documents created by release_planned_order), oldest first.",
            usage: "erpcraft list_purchase_orders",
            primary_arg: None,
            input_schema: no_args.clone(),
        },
        CommandSpec {
            name: "receive_purchase_order",
            summary: "Record a purchase order's arrival: its quantity moves from on-order to on-hand. (Quantity event only — the GL side is pay_supplier when the bill is paid.)",
            usage: "erpcraft receive_purchase_order <purchase_order_id>",
            primary_arg: Some("purchase_order_id"),
            input_schema: schema(json!({ "purchase_order_id": id_prop("Purchase order id") }), &["purchase_order_id"]),
        },
        CommandSpec {
            name: "complete_production_order",
            summary: "Record a production order's completion: its quantity moves from WIP to on-hand finished stock. (Quantity event only — the GL side is the complete_production builder.)",
            usage: "erpcraft complete_production_order <production_order_id>",
            primary_arg: Some("production_order_id"),
            input_schema: schema(json!({ "production_order_id": id_prop("Production order id") }), &["production_order_id"]),
        },
        CommandSpec {
            name: "list_customers",
            summary: "All customers, in seed order.",
            usage: "erpcraft list_customers",
            primary_arg: None,
            input_schema: no_args.clone(),
        },
        CommandSpec {
            name: "list_items",
            summary: "All sellable items with their prices and standard costs.",
            usage: "erpcraft list_items",
            primary_arg: None,
            input_schema: no_args.clone(),
        },
        CommandSpec {
            name: "list_quotes",
            summary: "All quotes with lines and derived totals.",
            usage: "erpcraft list_quotes",
            primary_arg: None,
            input_schema: no_args.clone(),
        },
        CommandSpec {
            name: "get_quote",
            summary: "One quote with lines and derived total, by id.",
            usage: "erpcraft get_quote <quote_id>",
            primary_arg: Some("quote_id"),
            input_schema: schema(json!({ "quote_id": id_prop("Quote id") }), &["quote_id"]),
        },
        CommandSpec {
            name: "list_orders",
            summary: "All sales orders with lines and derived totals.",
            usage: "erpcraft list_orders",
            primary_arg: None,
            input_schema: no_args.clone(),
        },
        CommandSpec {
            name: "get_order",
            summary: "One sales order with lines and derived total, by id.",
            usage: "erpcraft get_order <order_id>",
            primary_arg: Some("order_id"),
            input_schema: schema(json!({ "order_id": id_prop("Sales order id") }), &["order_id"]),
        },
        CommandSpec {
            name: "list_invoices",
            summary: "All invoices with lines, totals, amounts paid and balances.",
            usage: "erpcraft list_invoices",
            primary_arg: None,
            input_schema: no_args.clone(),
        },
        CommandSpec {
            name: "get_invoice",
            summary: "One invoice with lines, total, amount paid and balance, by id.",
            usage: "erpcraft get_invoice <invoice_id>",
            primary_arg: Some("invoice_id"),
            input_schema: schema(json!({ "invoice_id": id_prop("Invoice id") }), &["invoice_id"]),
        },
        CommandSpec {
            name: "list_payments",
            summary: "All recorded payments.",
            usage: "erpcraft list_payments",
            primary_arg: None,
            input_schema: no_args.clone(),
        },
        CommandSpec {
            name: "list_payments_for_invoice",
            summary: "Payments recorded against one invoice, oldest first.",
            usage: "erpcraft list_payments_for_invoice <invoice_id>",
            primary_arg: Some("invoice_id"),
            input_schema: schema(
                json!({ "invoice_id": id_prop("Invoice id") }),
                &["invoice_id"],
            ),
        },
        // -- Order-to-cash -------------------------------------------
        CommandSpec {
            name: "create_quote",
            summary: "Create a draft quote. Line prices snapshot from the item master; a line may carry a variant configuration (characteristic code → value), which is evaluated and roll-up priced by the configurator.",
            usage: "erpcraft create_quote --json '{\"customer_id\":1,\"lines\":[{\"item_id\":1,\"qty\":40}]}'",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "customer_id": id_prop("Customer id"),
                    "lines": {
                        "type": "array",
                        "description": "Quote lines: item_id, qty, and an optional configuration object (characteristic code → value)",
                        "items": {
                            "type": "object",
                            "properties": {
                                "item_id": { "type": "integer" },
                                "qty": { "type": "integer" },
                                "configuration": { "type": "object" }
                            },
                            "required": ["item_id", "qty"]
                        }
                    }
                }),
                &["customer_id", "lines"],
            ),
        },
        CommandSpec {
            name: "send_quote",
            summary: "Send a draft quote (draft → sent).",
            usage: "erpcraft send_quote <quote_id>",
            primary_arg: Some("quote_id"),
            input_schema: schema(json!({ "quote_id": id_prop("Quote id") }), &["quote_id"]),
        },
        CommandSpec {
            name: "accept_quote",
            summary: "Accept a sent quote (sent → accepted). Configured lines must evaluate consistent.",
            usage: "erpcraft accept_quote <quote_id>",
            primary_arg: Some("quote_id"),
            input_schema: schema(json!({ "quote_id": id_prop("Quote id") }), &["quote_id"]),
        },
        CommandSpec {
            name: "decline_quote",
            summary: "Decline a sent quote (sent → declined).",
            usage: "erpcraft decline_quote <quote_id>",
            primary_arg: Some("quote_id"),
            input_schema: schema(json!({ "quote_id": id_prop("Quote id") }), &["quote_id"]),
        },
        CommandSpec {
            name: "expire_quote",
            summary: "Expire a sent quote (sent → expired).",
            usage: "erpcraft expire_quote <quote_id>",
            primary_arg: Some("quote_id"),
            input_schema: schema(json!({ "quote_id": id_prop("Quote id") }), &["quote_id"]),
        },
        CommandSpec {
            name: "create_order",
            summary: "Create a sales order from an accepted quote, copying its customer and lines.",
            usage: "erpcraft create_order <quote_id>",
            primary_arg: Some("quote_id"),
            input_schema: schema(json!({ "quote_id": id_prop("Accepted quote id") }), &["quote_id"]),
        },
        CommandSpec {
            name: "fulfil_order",
            summary: "Fulfil an open sales order (open → fulfilled).",
            usage: "erpcraft fulfil_order <order_id>",
            primary_arg: Some("order_id"),
            input_schema: schema(json!({ "order_id": id_prop("Sales order id") }), &["order_id"]),
        },
        CommandSpec {
            name: "cancel_order",
            summary: "Cancel an open sales order (open → cancelled).",
            usage: "erpcraft cancel_order <order_id>",
            primary_arg: Some("order_id"),
            input_schema: schema(json!({ "order_id": id_prop("Sales order id") }), &["order_id"]),
        },
        CommandSpec {
            name: "create_invoice",
            summary: "Create a draft invoice from a sales order. A separate step from issuing: the draft is visible before anything posts.",
            usage: "erpcraft create_invoice <order_id>",
            primary_arg: Some("order_id"),
            input_schema: schema(json!({ "order_id": id_prop("Sales order id") }), &["order_id"]),
        },
        CommandSpec {
            name: "issue_invoice",
            summary: "Issue a draft invoice (draft → issued): posts revenue (Dr AR / Cr Revenue) and COGS (Dr COGS / Cr Finished Goods, from the line cost snapshots) in the same transaction. Returns the invoice and both posted entries.",
            usage: "erpcraft issue_invoice <invoice_id>",
            primary_arg: Some("invoice_id"),
            input_schema: schema(json!({ "invoice_id": id_prop("Invoice id") }), &["invoice_id"]),
        },
        CommandSpec {
            name: "record_payment",
            summary: "Record a payment against an issued invoice; posts Dr Cash / Cr AR and advances the invoice toward paid. Overpayment is refused. Returns the payment and its posted entry. Above the configured approval threshold, actor and a different approver are required (controls).",
            usage: "erpcraft record_payment --invoice-id <id> --amount-cents <cents>",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "invoice_id": id_prop("Invoice id"),
                    "amount_cents": { "type": "integer", "description": "Payment amount in cents (positive)" },
                    "actor": { "type": "string", "description": "Who is recording the payment (required above the approval threshold)" },
                    "approver": { "type": "string", "description": "Second approver, a different person with role approver/admin (required above the approval threshold); recorded on the payment and its entry" }
                }),
                &["invoice_id", "amount_cents"],
            ),
        },
        CommandSpec {
            name: "record_return",
            summary: "Record a sales return against an issued invoice: posts two reversing entries (Dr Sales Returns / Cr AR at snapshotted prices; Dr Finished Goods / Cr COGS at snapshotted costs), each pointing at the entry it corrects. Returns both entries.",
            usage: "erpcraft record_return --json '{\"invoice_id\":1,\"lines\":[{\"item_id\":1,\"qty\":2}]}'",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "invoice_id": id_prop("Invoice id"),
                    "lines": {
                        "type": "array",
                        "description": "Returned lines: item_id and qty",
                        "items": {
                            "type": "object",
                            "properties": {
                                "item_id": { "type": "integer" },
                                "qty": { "type": "integer" }
                            },
                            "required": ["item_id", "qty"]
                        }
                    }
                }),
                &["invoice_id", "lines"],
            ),
        },
        // -- Manual journal ------------------------------------------
        CommandSpec {
            name: "post_entry",
            summary: "Post a raw journal entry (the CLI/MCP equivalent of POST /journal-entries): entry_date, memo and two or more lines; the entry's source tag is set by the door. The database enforces debits == credits.",
            usage: "erpcraft post_entry --json '{\"entry_date\":\"2026-10-29\",\"memo\":\"…\",\"lines\":[{\"account_code\":\"1000\",\"debit_cents\":100},{\"account_code\":\"4000\",\"credit_cents\":100}]}'",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "entry_date": { "type": "string", "description": "YYYY-MM-DD" },
                    "memo": { "type": "string" },
                    "reverses_entry_id": { "type": "integer", "description": "The entry this one corrects, when it is a correction" },
                    "lines": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "account_code": { "type": "string" },
                                "debit_cents": { "type": "integer" },
                                "credit_cents": { "type": "integer" }
                            },
                            "required": ["account_code"]
                        }
                    }
                }),
                &["entry_date", "memo", "lines"],
            ),
        },
    ]
}

/// The manual-journal builders (docs/DAY4_PREP.md): the agent-contract
/// spec's named intents have no dedicated core functions — on the
/// oracle day they are plain `post_entry` calls with fixed account
/// pairs. Each builder here constructs exactly one two-line
/// `NewEntry` and calls the one `post_entry`; no builder writes SQL,
/// computes a balance, or bypasses the posting path.
///
/// (name, memo, debit account, credit account)
const BUILDERS: &[(&str, &str, &str, &str)] = &[
    ("buy_materials", "Buy materials", "1200", "2000"),
    ("pay_supplier", "Pay supplier", "2000", "1000"),
    (
        "issue_materials",
        "Issue materials to production",
        "1210",
        "1200",
    ),
    ("record_labor", "Record direct labor", "1210", "1000"),
    ("complete_production", "Complete production", "1220", "1210"),
    ("record_deposit", "Record customer deposit", "1000", "2200"),
    ("record_rent", "Record rent", "6000", "1000"),
    ("record_interest", "Record interest", "6010", "1000"),
    ("record_depreciation", "Record depreciation", "6020", "1310"),
];

fn builder_spec(name: &'static str) -> CommandSpec {
    let (summary, usage) = match name {
        "buy_materials" => (
            "Buy materials: post Dr 1200 Raw Materials / Cr 2000 Accounts Payable for amount_cents (a thin builder over post_entry).",
            "erpcraft buy_materials --amount-cents <cents>",
        ),
        "pay_supplier" => (
            "Pay supplier: post Dr 2000 Accounts Payable / Cr 1000 Cash for amount_cents (a thin builder over post_entry).",
            "erpcraft pay_supplier --amount-cents <cents>",
        ),
        "issue_materials" => (
            "Issue materials to production: post Dr 1210 WIP / Cr 1200 Raw Materials for amount_cents (a thin builder over post_entry).",
            "erpcraft issue_materials --amount-cents <cents>",
        ),
        "record_labor" => (
            "Record direct labor: post Dr 1210 WIP / Cr 1000 Cash for amount_cents (a thin builder over post_entry).",
            "erpcraft record_labor --amount-cents <cents>",
        ),
        "complete_production" => (
            "Complete production: post Dr 1220 Finished Goods Inventory / Cr 1210 Work in Process for amount_cents (a thin builder over post_entry). The units finished are proposal text — the posting is value-only, exactly like the oracle day's T7.",
            "erpcraft complete_production --amount-cents <cents>",
        ),
        "record_deposit" => (
            "Record customer deposit: post Dr 1000 Cash / Cr 2200 Customer Deposits for amount_cents (a thin builder over post_entry).",
            "erpcraft record_deposit --amount-cents <cents>",
        ),
        "record_rent" => (
            "Record rent: post Dr 6000 Rent Expense / Cr 1000 Cash for amount_cents (a thin builder over post_entry).",
            "erpcraft record_rent --amount-cents <cents>",
        ),
        "record_interest" => (
            "Record interest: post Dr 6010 Interest Expense / Cr 1000 Cash for amount_cents (a thin builder over post_entry).",
            "erpcraft record_interest --amount-cents <cents>",
        ),
        "record_depreciation" => (
            "Record depreciation: post Dr 6020 Depreciation Expense / Cr 1310 Accumulated Depreciation for amount_cents (a thin builder over post_entry).",
            "erpcraft record_depreciation --amount-cents <cents>",
        ),
        _ => unreachable!("builder_spec called for a non-builder"),
    };
    CommandSpec {
        name,
        summary,
        usage,
        primary_arg: None,
        input_schema: schema(
            json!({
                "amount_cents": { "type": "integer", "description": "Amount in cents (positive)" },
                "entry_date": { "type": "string", "description": "YYYY-MM-DD; defaults to today (UTC)" }
            }),
            &["amount_cents"],
        ),
    }
}

/// Configurator authoring commands (the prep's stretch set — shipped
/// because the door is a straight pass-through to the core functions
/// the HTTP surface already exposes; evaluation stays the read door).
fn configurator_specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            name: "list_config_models",
            summary: "All variant configuration models with their characteristics, values, components and dependencies.",
            usage: "erpcraft list_config_models",
            primary_arg: None,
            input_schema: schema(json!({}), &[]),
        },
        CommandSpec {
            name: "get_config_model",
            summary: "One configuration model by id, with characteristics, components and dependencies.",
            usage: "erpcraft get_config_model <model_id>",
            primary_arg: Some("model_id"),
            input_schema: schema(
                json!({ "model_id": id_prop("Configuration model id") }),
                &["model_id"],
            ),
        },
        CommandSpec {
            name: "evaluate_config",
            summary: "Evaluate a (partial) configuration against a model: verdict, selectable values, forced values/components and the structured explanations for every outcome. An inconsistent verdict is a refusal carrying the explanations.",
            usage: "erpcraft evaluate_config --json '{\"model_id\":1,\"configuration\":{\"material\":\"steel\",\"finish\":\"galvanized\"}}'",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "model_id": id_prop("Configuration model id"),
                    "configuration": {
                        "type": "object",
                        "description": "Characteristic code → value (string code, integer, or boolean)"
                    }
                }),
                &["model_id", "configuration"],
            ),
        },
        CommandSpec {
            name: "create_config_model",
            summary: "Create a variant configuration model for an item (stretch authoring command; same core function as the HTTP surface).",
            usage: "erpcraft create_config_model --json '{\"item_id\":1,\"name\":\"…\"}'",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "item_id": id_prop("Item id"),
                    "name": { "type": "string" }
                }),
                &["item_id", "name"],
            ),
        },
        CommandSpec {
            name: "add_characteristic",
            summary: "Add a characteristic (with its enum values, for enum type) to a configuration model.",
            usage: "erpcraft add_characteristic --json '{\"model_id\":1,\"code\":\"finish\",\"name\":\"Finish\",\"value_type\":\"enum\",\"values\":[{\"code\":\"bare\",\"label\":\"Bare\"}]}'",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "model_id": id_prop("Configuration model id"),
                    "code": { "type": "string" },
                    "name": { "type": "string" },
                    "value_type": { "type": "string", "description": "enum | integer | bool" },
                    "min_value": { "type": "integer" },
                    "max_value": { "type": "integer" },
                    "required": { "type": "boolean" },
                    "values": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "code": { "type": "string" },
                                "label": { "type": "string" }
                            },
                            "required": ["code", "label"]
                        }
                    }
                }),
                &["model_id", "code", "name", "value_type"],
            ),
        },
        CommandSpec {
            name: "create_component",
            summary: "Add a component (with price/cost deltas) to a configuration model.",
            usage: "erpcraft create_component --json '{\"model_id\":1,\"code\":\"…\",\"name\":\"…\",\"price_delta_cents\":0,\"cost_delta_cents\":0}'",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "model_id": id_prop("Configuration model id"),
                    "code": { "type": "string" },
                    "name": { "type": "string" },
                    "component_item_id": { "type": "integer" },
                    "qty": { "type": "integer" },
                    "price_delta_cents": { "type": "integer" },
                    "cost_delta_cents": { "type": "integer" }
                }),
                &["model_id", "code", "name", "price_delta_cents", "cost_delta_cents"],
            ),
        },
        CommandSpec {
            name: "create_dependency",
            summary: "Add a typed dependency (precondition | selection | constraint) to a configuration model. The expression is a typed AST as JSON, never source text; cycles are rejected at save.",
            usage: "erpcraft create_dependency --json '{\"model_id\":1,\"kind\":\"precondition\",\"target\":\"finish=galvanized\",\"expr\":{\"type\":\"eq\",\"char\":\"material\",\"value\":\"steel\"},\"source_text\":\"…\"}'",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "model_id": id_prop("Configuration model id"),
                    "kind": { "type": "string", "description": "precondition | selection | constraint" },
                    "target": { "type": "string" },
                    "expr": { "type": "object", "description": "Typed expression AST (see the core Expr type)" },
                    "source_text": { "type": "string" },
                    "created_by": { "type": "string" }
                }),
                &["model_id", "kind", "target", "expr"],
            ),
        },
    ]
}

/// Procure-to-pay commands (ENTERPRISE_GAPS.md §3): the supplier
/// chain mirroring order-to-cash — suppliers, requisitions,
/// procurement orders, goods receipts, AP invoices with three-way
/// match, AP payments. (Distinct from MRP's planning purchase
/// orders: `list_purchase_orders` is the plan; these are the
/// commercial documents.)
fn procurement_specs() -> Vec<CommandSpec> {
    let line_items = |desc: &str, price_desc: &str| {
        json!({
            "type": "array",
            "description": desc,
            "items": {
                "type": "object",
                "properties": {
                    "item_id": { "type": "integer" },
                    "qty": { "type": "integer" },
                    "unit_price_cents": { "type": "integer", "description": price_desc }
                },
                "required": ["item_id", "qty", "unit_price_cents"]
            }
        })
    };
    vec![
        CommandSpec {
            name: "list_suppliers",
            summary: "All suppliers.",
            usage: "erpcraft list_suppliers",
            primary_arg: None,
            input_schema: schema(json!({}), &[]),
        },
        CommandSpec {
            name: "create_supplier",
            summary: "Create a supplier by name (names are unique).",
            usage: "erpcraft create_supplier --name 'SteelWorks Supply'",
            primary_arg: None,
            input_schema: schema(json!({ "name": { "type": "string" } }), &["name"]),
        },
        CommandSpec {
            name: "create_requisition",
            summary: "Create a draft purchase requisition: optional supplier, requester, and lines at estimated prices. Approved requisitions convert to purchase orders.",
            usage: "erpcraft create_requisition --json '{\"supplier_id\":1,\"requested_by\":\"ops\",\"lines\":[{\"item_id\":2,\"qty\":100,\"unit_price_cents\":1800}]}'",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "supplier_id": id_prop("Supplier id (optional)"),
                    "requested_by": { "type": "string", "description": "Who is asking (recorded on the requisition)" },
                    "lines": line_items("Requisition lines: item_id, qty, estimated unit price", "Estimated unit price in cents")
                }),
                &["lines"],
            ),
        },
        CommandSpec {
            name: "list_requisitions",
            summary: "All purchase requisitions with their lines and totals.",
            usage: "erpcraft list_requisitions",
            primary_arg: None,
            input_schema: schema(json!({}), &[]),
        },
        CommandSpec {
            name: "get_requisition",
            summary: "One purchase requisition by id, with lines and total.",
            usage: "erpcraft get_requisition <requisition_id>",
            primary_arg: Some("requisition_id"),
            input_schema: schema(
                json!({ "requisition_id": id_prop("Requisition id") }),
                &["requisition_id"],
            ),
        },
        CommandSpec {
            name: "approve_requisition",
            summary: "Approve a draft purchase requisition (draft → approved).",
            usage: "erpcraft approve_requisition <requisition_id>",
            primary_arg: Some("requisition_id"),
            input_schema: schema(
                json!({ "requisition_id": id_prop("Requisition id") }),
                &["requisition_id"],
            ),
        },
        CommandSpec {
            name: "reject_requisition",
            summary: "Reject a draft purchase requisition (draft → rejected).",
            usage: "erpcraft reject_requisition <requisition_id>",
            primary_arg: Some("requisition_id"),
            input_schema: schema(
                json!({ "requisition_id": id_prop("Requisition id") }),
                &["requisition_id"],
            ),
        },
        CommandSpec {
            name: "cancel_requisition",
            summary: "Cancel a draft or approved purchase requisition.",
            usage: "erpcraft cancel_requisition <requisition_id>",
            primary_arg: Some("requisition_id"),
            input_schema: schema(
                json!({ "requisition_id": id_prop("Requisition id") }),
                &["requisition_id"],
            ),
        },
        CommandSpec {
            name: "create_purchase_order",
            summary: "Create a procurement order with a supplier. With requisition_id (an approved requisition), supplier and lines default to the requisition's and the requisition converts. Distinct from MRP planning purchase orders.",
            usage: "erpcraft create_purchase_order --json '{\"supplier_id\":1,\"lines\":[{\"item_id\":2,\"qty\":100,\"unit_price_cents\":1800}]}'",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "supplier_id": id_prop("Supplier id (optional when requisition_id names one)"),
                    "requisition_id": id_prop("Approved requisition id (optional)"),
                    "lines": line_items("Order lines: item_id, qty, agreed unit price (optional when requisition_id supplies them)", "Agreed unit price in cents")
                }),
                &[],
            ),
        },
        CommandSpec {
            name: "list_procurement_orders",
            summary: "All procurement orders with lines, ordered and received totals.",
            usage: "erpcraft list_procurement_orders",
            primary_arg: None,
            input_schema: schema(json!({}), &[]),
        },
        CommandSpec {
            name: "get_procurement_order",
            summary: "One procurement order by id, with lines and received quantities.",
            usage: "erpcraft get_procurement_order <order_id>",
            primary_arg: Some("order_id"),
            input_schema: schema(
                json!({ "order_id": id_prop("Procurement order id") }),
                &["order_id"],
            ),
        },
        CommandSpec {
            name: "cancel_purchase_order",
            summary: "Cancel an open procurement order with nothing received against it.",
            usage: "erpcraft cancel_purchase_order <order_id>",
            primary_arg: Some("order_id"),
            input_schema: schema(
                json!({ "order_id": id_prop("Procurement order id") }),
                &["order_id"],
            ),
        },
        CommandSpec {
            name: "receive_goods",
            summary: "Receive goods against a procurement order (partial receipts allowed; over-receipt refused). Posts Dr inventory / Cr 2050 GR-IR at PO prices and moves stock on hand. Returns the receipt and its posted entry.",
            usage: "erpcraft receive_goods --json '{\"order_id\":1,\"lines\":[{\"po_line_id\":1,\"qty\":60}]}'",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "order_id": id_prop("Procurement order id"),
                    "lines": {
                        "type": "array",
                        "description": "Received lines: po_line_id and qty arrived",
                        "items": {
                            "type": "object",
                            "properties": {
                                "po_line_id": { "type": "integer" },
                                "qty": { "type": "integer" }
                            },
                            "required": ["po_line_id", "qty"]
                        }
                    }
                }),
                &["order_id", "lines"],
            ),
        },
        CommandSpec {
            name: "list_goods_receipts",
            summary: "Goods receipts, optionally filtered to one procurement order.",
            usage: "erpcraft list_goods_receipts [--order-id <id>]",
            primary_arg: None,
            input_schema: schema(
                json!({ "order_id": id_prop("Procurement order id (optional filter)") }),
                &[],
            ),
        },
        CommandSpec {
            name: "record_ap_invoice",
            summary: "Record a supplier invoice against a procurement order: posts Dr 2050 GR-IR at PO prices, the price difference to 6040 PPV, Cr 2000 AP at the invoice total, then runs the three-way match — variances land the invoice on_hold (recorded, not payable). Returns the AP invoice and its posted entry.",
            usage: "erpcraft record_ap_invoice --json '{\"order_id\":1,\"lines\":[{\"po_line_id\":1,\"qty\":60,\"unit_price_cents\":1800}]}'",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "order_id": id_prop("Procurement order id"),
                    "lines": {
                        "type": "array",
                        "description": "Invoiced lines: po_line_id, qty billed, unit price billed",
                        "items": {
                            "type": "object",
                            "properties": {
                                "po_line_id": { "type": "integer" },
                                "qty": { "type": "integer" },
                                "unit_price_cents": { "type": "integer" }
                            },
                            "required": ["po_line_id", "qty", "unit_price_cents"]
                        }
                    }
                }),
                &["order_id", "lines"],
            ),
        },
        CommandSpec {
            name: "list_ap_invoices",
            summary: "All AP invoices with lines, live match variances, totals and balances.",
            usage: "erpcraft list_ap_invoices",
            primary_arg: None,
            input_schema: schema(json!({}), &[]),
        },
        CommandSpec {
            name: "get_ap_invoice",
            summary: "One AP invoice by id, with its three-way-match variances and any recorded match override.",
            usage: "erpcraft get_ap_invoice <ap_invoice_id>",
            primary_arg: Some("ap_invoice_id"),
            input_schema: schema(
                json!({ "ap_invoice_id": id_prop("AP invoice id") }),
                &["ap_invoice_id"],
            ),
        },
        CommandSpec {
            name: "override_ap_match",
            summary: "Override a three-way-match hold with a recorded reason and the overriding person's name; both are stored on the AP invoice. The invoice becomes payable; the variance facts stay visible.",
            usage: "erpcraft override_ap_match --ap-invoice-id <id> --reason '…' --by '…'",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "ap_invoice_id": id_prop("AP invoice id"),
                    "reason": { "type": "string", "description": "Why the variance is accepted (recorded)" },
                    "by": { "type": "string", "description": "Who is overriding (recorded)" }
                }),
                &["ap_invoice_id", "reason", "by"],
            ),
        },
        CommandSpec {
            name: "pay_ap_invoice",
            summary: "Pay (part of) an open AP invoice: posts Dr 2000 AP / Cr 1000 Cash and advances the invoice toward paid. On-hold (match variance) and overpayment are refused. Returns the payment and its posted entry. Above the configured approval threshold, actor and a different approver are required (controls).",
            usage: "erpcraft pay_ap_invoice --ap-invoice-id <id> --amount-cents <cents>",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "ap_invoice_id": id_prop("AP invoice id"),
                    "amount_cents": { "type": "integer", "description": "Payment amount in cents (positive)" },
                    "actor": { "type": "string", "description": "Who is making the payment (required above the approval threshold)" },
                    "approver": { "type": "string", "description": "Second approver, a different person with role approver/admin (required above the approval threshold); recorded on the payment and its entry" }
                }),
                &["ap_invoice_id", "amount_cents"],
            ),
        },
        CommandSpec {
            name: "list_ap_payments",
            summary: "AP payments, optionally filtered to one AP invoice.",
            usage: "erpcraft list_ap_payments [--ap-invoice-id <id>]",
            primary_arg: None,
            input_schema: schema(
                json!({ "ap_invoice_id": id_prop("AP invoice id (optional filter)") }),
                &[],
            ),
        },
    ]
}

/// Controls commands (ENTERPRISE_GAPS.md §6/§7): fiscal periods,
/// users/roles, and the payment approval threshold.
fn controls_specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            name: "list_fiscal_periods",
            summary: "All fiscal periods with their date ranges and open/closed status.",
            usage: "erpcraft list_fiscal_periods",
            primary_arg: None,
            input_schema: schema(json!({}), &[]),
        },
        CommandSpec {
            name: "create_fiscal_period",
            summary: "Create an open fiscal period: a unique name and an inclusive ISO date range; ranges may not overlap existing periods.",
            usage: "erpcraft create_fiscal_period --name 2026-11 --start-date 2026-11-01 --end-date 2026-11-30",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "name": { "type": "string", "description": "Period name, e.g. '2026-11'" },
                    "start_date": { "type": "string", "description": "First day, YYYY-MM-DD" },
                    "end_date": { "type": "string", "description": "Last day, YYYY-MM-DD" }
                }),
                &["name", "start_date", "end_date"],
            ),
        },
        CommandSpec {
            name: "close_fiscal_period",
            summary: "Close a fiscal period: every posting path then refuses entries dated inside it. One-way in this build.",
            usage: "erpcraft close_fiscal_period <period_id>",
            primary_arg: Some("period_id"),
            input_schema: schema(
                json!({ "period_id": id_prop("Fiscal period id") }),
                &["period_id"],
            ),
        },
        CommandSpec {
            name: "list_users",
            summary: "All users in the controls registry, with roles (admin / approver / clerk).",
            usage: "erpcraft list_users",
            primary_arg: None,
            input_schema: schema(json!({}), &[]),
        },
        CommandSpec {
            name: "create_user",
            summary: "Create a user in the controls registry (names are unique; role is admin, approver, or clerk). Identities are names, not logins — see controls.rs.",
            usage: "erpcraft create_user --name sam --role approver",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "name": { "type": "string" },
                    "role": { "type": "string", "description": "admin | approver | clerk" }
                }),
                &["name", "role"],
            ),
        },
        CommandSpec {
            name: "list_control_settings",
            summary: "All control settings (key/value), including the payment approval threshold when set.",
            usage: "erpcraft list_control_settings",
            primary_arg: None,
            input_schema: schema(json!({}), &[]),
        },
        CommandSpec {
            name: "set_approval_threshold",
            summary: "Set the payment approval threshold in cents: payments above it (AR receipts and AP payments) require an actor and a different approver with role approver/admin, recorded on the payment and its entry.",
            usage: "erpcraft set_approval_threshold --threshold-cents 100000",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "threshold_cents": { "type": "integer", "description": "Threshold in cents (payments strictly above need approval)" }
                }),
                &["threshold_cents"],
            ),
        },
    ]
}

/// The full dispatch table, in door order: reads, order-to-cash,
/// manual journal, builders, configurator, procure-to-pay, controls.
pub fn all_specs() -> Vec<CommandSpec> {
    let mut out = specs();
    for (name, _, _, _) in BUILDERS {
        out.push(builder_spec(name));
    }
    out.extend(configurator_specs());
    out.extend(procurement_specs());
    out.extend(controls_specs());
    out
}

pub fn spec_for(name: &str) -> Option<CommandSpec> {
    all_specs().into_iter().find(|s| s.name == name)
}

// ---------------------------------------------------------------------------
// Argument helpers — every failure is a plain-language refusal.
// ---------------------------------------------------------------------------

fn req_i64(args: &Value, key: &str) -> Result<i64, DispatchError> {
    args.get(key).and_then(|v| v.as_i64()).ok_or_else(|| {
        DispatchError::refused(format!(
            "argument '{key}' is required and must be an integer"
        ))
    })
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

/// Account codes are numeric strings ("1100"); accept either the JSON
/// string or a bare number so `account_ledger 1100` works naturally.
fn req_code(args: &Value, key: &str) -> Result<String, DispatchError> {
    match args.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        Some(Value::Number(n)) if n.as_i64().is_some() => Ok(n.to_string()),
        _ => Err(DispatchError::refused(format!(
            "argument '{key}' is required and must be an account code"
        ))),
    }
}

fn parse_as<T>(args: &Value, what: &str) -> Result<T, DispatchError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(args.clone())
        .map_err(|e| DispatchError::refused(format!("invalid {what} arguments: {e}")))
}

fn to_json<T: serde::Serialize>(value: T) -> DispatchResult {
    serde_json::to_value(value)
        .map_err(|e| DispatchError::internal(format!("could not serialize the result: {e}")))
}

/// Today (UTC) as `YYYY-MM-DD` — the default date for builder entries.
/// (Howard Hinnant's civil-from-days algorithm; unit-tested below.)
fn today_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    civil_from_days((secs / 86_400) as i64)
}

fn civil_from_days(z: i64) -> String {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Open the ledger database exactly as the HTTP server boots:
/// `ERPCRAFT_DB` selects the path (same default), and `open_db` runs
/// the schema + all three seed steps.
pub fn open_door_db() -> Result<Connection, DispatchError> {
    let db_path = std::env::var("ERPCRAFT_DB").unwrap_or_else(|_| "erpcraft.db".to_string());
    let conn = open_db(&db_path).map_err(|e| {
        DispatchError::internal(format!("cannot open the ledger database at {db_path}: {e}"))
    })?;
    // MRP demo scenario data (BOM, stock, lead times) is seeded at
    // the doors, not in open_db — see MRP_DESIGN.md.
    erpcraft_core::seed_mrp_demo_if_empty(&conn)
        .map_err(|e| DispatchError::internal(format!("cannot seed the MRP demo scenario: {e}")))?;
    // Procure-to-pay seed (GR-IR/PPV accounts + demo suppliers) is
    // likewise door-level — see procurement.rs for why.
    erpcraft_core::seed_procurement_if_empty(&conn).map_err(|e| {
        DispatchError::internal(format!("cannot seed the procure-to-pay data: {e}"))
    })?;
    Ok(conn)
}

/// Run one command by name against an open database. This is the
/// single entry point both doors call.
pub fn dispatch(conn: &mut Connection, door: Door, name: &str, args: &Value) -> DispatchResult {
    match name {
        // Reads
        "health" => cmd_health(conn),
        "trial_balance" => to_json(trial_balance(conn)?),
        "account_ledger" => to_json(account_ledger(conn, &req_code(args, "code")?)?),
        "get_entry" => {
            let id = req_i64(args, "entry_id")?;
            match get_entry(conn, id)? {
                Some(entry) => to_json(entry),
                None => Err(DispatchError::refused(format!(
                    "no journal entry with id {id}"
                ))),
            }
        }
        "list_entries" => to_json(list_entries(conn)?),
        "audit_entry" => {
            let id = req_i64(args, "entry_id")?;
            match audit_entry(conn, id)? {
                Some(record) => to_json(record),
                None => Err(DispatchError::refused(format!(
                    "no journal entry with id {id}"
                ))),
            }
        }
        "audit_trail" => {
            let filter = AuditFilter {
                source: opt_str(args, "source").map(|s| s.to_string()),
                document: opt_str(args, "document").map(|s| s.to_string()),
                from_date: opt_str(args, "from_date").map(|s| s.to_string()),
                to_date: opt_str(args, "to_date").map(|s| s.to_string()),
            };
            to_json(audit_trail(conn, &filter)?)
        }
        "run_mrp" => {
            let as_of = mrp_as_of(args);
            let demand = mrp_demand_from_args(args)?;
            to_json(run_mrp(conn, &as_of, &demand)?)
        }
        "list_planned_orders" => {
            let as_of = mrp_as_of(args);
            let demand = mrp_demand_from_args(args)?;
            let plan = run_mrp(conn, &as_of, &demand)?;
            Ok(json!({
                "as_of": plan.as_of,
                "planned_orders": plan.planned_orders,
                "shortages": plan.shortages,
            }))
        }
        "explain_shortage" => {
            let sku = req_str(args, "sku")?.to_string();
            if !list_stock(conn)?.iter().any(|s| s.sku == sku) {
                return Err(DispatchError::refused(format!("no item with SKU {sku}")));
            }
            let as_of = mrp_as_of(args);
            let demand = mrp_demand_from_args(args)?;
            let plan = run_mrp(conn, &as_of, &demand)?;
            let planned = plan.planned_orders.iter().find(|o| o.sku == sku).cloned();
            let netting = plan.requirements.iter().find(|r| r.sku == sku);
            let (shortage, explanation) = match (&planned, netting) {
                (Some(o), _) if o.days_late > 0 => (
                    true,
                    format!(
                        "{} is short {} units: demand is {} by {}, available supply is {} \
                         (on hand {} + on order {} + WIP {}), so a {} order for {} must \
                         start {} — {} day(s) before this plan's as-of date {}.",
                        sku,
                        o.qty,
                        o.gross_required,
                        o.due_date,
                        o.on_hand + o.on_order + o.wip,
                        o.on_hand,
                        o.on_order,
                        o.wip,
                        o.kind,
                        o.qty,
                        o.start_date,
                        o.days_late,
                        plan.as_of
                    ),
                ),
                (Some(o), _) => (
                    false,
                    format!(
                        "{} has a planned {} order for {} due {} starting {} — it starts \
                         on or after the as-of date {}, so it is planned, not short.",
                        sku, o.kind, o.qty, o.due_date, o.start_date, plan.as_of
                    ),
                ),
                (None, Some(n)) => (
                    false,
                    format!(
                        "{} is not short: demand {} is fully covered by {} available \
                         (on hand {} + on order {} + WIP {}).",
                        sku, n.gross_required, n.available, n.on_hand, n.on_order, n.wip
                    ),
                ),
                (None, None) => (
                    false,
                    format!("{sku} has no demand in the current plan — nothing is short."),
                ),
            };
            Ok(json!({
                "sku": sku,
                "as_of": plan.as_of,
                "shortage": shortage,
                "explanation": explanation,
                "planned_order": planned,
                "netting": netting,
            }))
        }
        "release_planned_order" => {
            let key = req_str(args, "key")?.to_string();
            let as_of = mrp_as_of(args);
            let demand = mrp_demand_from_args(args)?;
            to_json(release_planned_order(conn, &as_of, &demand, &key)?)
        }
        "list_stock" => to_json(list_stock(conn)?),
        "set_stock" => {
            let item_id = req_i64(args, "item_id")?;
            let on_hand = req_i64(args, "on_hand")?;
            let on_order = opt_i64(args, "on_order").unwrap_or(0);
            let wip = opt_i64(args, "wip").unwrap_or(0);
            to_json(set_stock(conn, item_id, on_hand, on_order, wip)?)
        }
        "list_bom" => to_json(list_bom(conn, req_i64(args, "parent_item_id")?)?),
        "add_bom_line" => to_json(add_bom_line(
            conn,
            req_i64(args, "parent_item_id")?,
            req_i64(args, "component_item_id")?,
            req_i64(args, "qty_per")?,
        )?),
        "list_production_orders" => to_json(list_production_orders(conn)?),
        "list_purchase_orders" => to_json(list_purchase_orders(conn)?),
        "receive_purchase_order" => to_json(receive_purchase_order(
            conn,
            req_i64(args, "purchase_order_id")?,
        )?),
        "complete_production_order" => to_json(complete_production_order(
            conn,
            req_i64(args, "production_order_id")?,
        )?),
        "list_customers" => to_json(list_customers(conn)?),
        "list_items" => to_json(list_items(conn)?),
        "list_quotes" => to_json(list_quotes(conn)?),
        "get_quote" => {
            let id = req_i64(args, "quote_id")?;
            match get_quote(conn, id)? {
                Some(quote) => to_json(quote),
                None => Err(DispatchError::refused(format!("no quote with id {id}"))),
            }
        }
        "list_orders" => to_json(list_orders(conn)?),
        "get_order" => {
            let id = req_i64(args, "order_id")?;
            match get_order(conn, id)? {
                Some(order) => to_json(order),
                None => Err(DispatchError::refused(format!(
                    "no sales order with id {id}"
                ))),
            }
        }
        "list_invoices" => to_json(list_invoices(conn)?),
        "get_invoice" => {
            let id = req_i64(args, "invoice_id")?;
            match get_invoice(conn, id)? {
                Some(invoice) => to_json(invoice),
                None => Err(DispatchError::refused(format!("no invoice with id {id}"))),
            }
        }
        "list_payments" => to_json(list_payments(conn)?),
        "list_payments_for_invoice" => to_json(list_payments_for_invoice(
            conn,
            req_i64(args, "invoice_id")?,
        )?),

        // Order-to-cash
        "create_quote" => {
            let new: NewQuote = parse_as(args, "create_quote")?;
            let id = create_quote(conn, &new)?;
            match get_quote(conn, id)? {
                Some(quote) => to_json(quote),
                None => Err(DispatchError::internal(format!(
                    "quote {id} was created but could not be read back"
                ))),
            }
        }
        "send_quote" => quote_transition(conn, args, send_quote),
        "accept_quote" => quote_transition(conn, args, accept_quote),
        "decline_quote" => quote_transition(conn, args, decline_quote),
        "expire_quote" => quote_transition(conn, args, expire_quote),
        "create_order" => {
            let id = create_order_from_quote(conn, req_i64(args, "quote_id")?)?;
            match get_order(conn, id)? {
                Some(order) => to_json(order),
                None => Err(DispatchError::internal(format!(
                    "sales order {id} was created but could not be read back"
                ))),
            }
        }
        "fulfil_order" => order_transition(conn, args, fulfil_order),
        "cancel_order" => order_transition(conn, args, cancel_order),
        "create_invoice" => {
            let id = create_invoice_from_order(conn, req_i64(args, "order_id")?)?;
            match get_invoice(conn, id)? {
                Some(invoice) => to_json(invoice),
                None => Err(DispatchError::internal(format!(
                    "invoice {id} was created but could not be read back"
                ))),
            }
        }
        "issue_invoice" => {
            let id = req_i64(args, "invoice_id")?;
            issue_invoice(conn, id)?;
            let invoice = get_invoice(conn, id)?.ok_or_else(|| {
                DispatchError::internal(format!("invoice {id} vanished after issuing"))
            })?;
            // The posting tools return the full posted entries, read
            // back — whatever drives the door can show exactly what
            // posted (the prep's one gate-adjacent rule).
            let revenue = entry_or_internal(conn, invoice.gl_entry_id, "revenue")?;
            let cogs = entry_or_internal(conn, invoice.cogs_entry_id, "COGS")?;
            Ok(json!({
                "invoice": serde_json::to_value(&invoice).unwrap_or(Value::Null),
                "revenue_entry": revenue,
                "cogs_entry": cogs,
            }))
        }
        "record_payment" => {
            let invoice_id = req_i64(args, "invoice_id")?;
            let amount = req_i64(args, "amount_cents")?;
            let actor = opt_str(args, "actor");
            let approver = opt_str(args, "approver");
            let payment_id = record_payment_attributed(conn, invoice_id, amount, actor, approver)?;
            let payment = list_payments_for_invoice(conn, invoice_id)?
                .into_iter()
                .find(|p| p.id == payment_id)
                .ok_or_else(|| {
                    DispatchError::internal(format!(
                        "payment {payment_id} was recorded but could not be read back"
                    ))
                })?;
            let entry = entry_or_internal(conn, payment.gl_entry_id, "payment")?;
            Ok(json!({
                "payment": serde_json::to_value(&payment).unwrap_or(Value::Null),
                "entry": entry,
            }))
        }
        "record_return" => {
            let invoice_id = req_i64(args, "invoice_id")?;
            let lines_value = args.get("lines").cloned().ok_or_else(|| {
                DispatchError::refused("argument 'lines' is required: an array of {item_id, qty}")
            })?;
            let lines: Vec<ReturnLine> = serde_json::from_value(lines_value)
                .map_err(|e| DispatchError::refused(format!("invalid record_return lines: {e}")))?;
            let (revenue_id, cogs_id) = record_sales_return(conn, invoice_id, &lines)?;
            let revenue = entry_or_internal(conn, Some(revenue_id), "return revenue")?;
            let cogs = entry_or_internal(conn, Some(cogs_id), "return COGS")?;
            Ok(json!({
                "revenue_entry": revenue,
                "cogs_entry": cogs,
            }))
        }

        // Manual journal
        "post_entry" => {
            let mut new: NewEntry = parse_as(args, "post_entry")?;
            // Provenance is set at the door, never by the caller.
            new.source = door.source_tag().to_string();
            let id = post_entry(conn, &new)?;
            match get_entry(conn, id)? {
                Some(entry) => to_json(entry),
                None => Err(DispatchError::internal(format!(
                    "entry {id} posted but could not be read back"
                ))),
            }
        }

        // Configurator door
        "list_config_models" => to_json(list_config_models(conn)?),
        "get_config_model" => {
            let id = req_i64(args, "model_id")?;
            match get_config_model(conn, id)? {
                Some(model) => to_json(model),
                None => Err(DispatchError::refused(format!(
                    "no config model with id {id}"
                ))),
            }
        }
        "evaluate_config" => cmd_evaluate_config(conn, args),
        "create_config_model" => {
            let new: erpcraft_core::NewConfigModel = parse_as(args, "create_config_model")?;
            let id = erpcraft_core::create_config_model(conn, &new)?;
            match get_config_model(conn, id)? {
                Some(model) => to_json(model),
                None => Err(DispatchError::internal(format!(
                    "config model {id} was created but could not be read back"
                ))),
            }
        }
        "add_characteristic" => {
            let model_id = req_i64(args, "model_id")?;
            let new: erpcraft_core::NewCharacteristic = parse_as(args, "add_characteristic")?;
            let id = erpcraft_core::add_characteristic(conn, model_id, &new)?;
            let model = get_config_model(conn, model_id)?.ok_or_else(|| {
                DispatchError::refused(format!("no config model with id {model_id}"))
            })?;
            let characteristic = model
                .characteristics
                .into_iter()
                .find(|c| c.id == id)
                .ok_or_else(|| {
                    DispatchError::internal(format!(
                        "characteristic {id} was added but could not be read back"
                    ))
                })?;
            to_json(characteristic)
        }
        "create_component" => {
            let model_id = req_i64(args, "model_id")?;
            let new: erpcraft_core::NewComponent = parse_as(args, "create_component")?;
            let id = erpcraft_core::create_component(conn, model_id, &new)?;
            let model = get_config_model(conn, model_id)?.ok_or_else(|| {
                DispatchError::refused(format!("no config model with id {model_id}"))
            })?;
            let component = model
                .components
                .into_iter()
                .find(|c| c.id == id)
                .ok_or_else(|| {
                    DispatchError::internal(format!(
                        "component {id} was created but could not be read back"
                    ))
                })?;
            to_json(component)
        }
        "create_dependency" => {
            let model_id = req_i64(args, "model_id")?;
            let new: erpcraft_core::NewDependency = parse_as(args, "create_dependency")?;
            let id = erpcraft_core::create_dependency(conn, model_id, &new)?;
            let model = get_config_model(conn, model_id)?.ok_or_else(|| {
                DispatchError::refused(format!("no config model with id {model_id}"))
            })?;
            let dependency = model
                .dependencies
                .into_iter()
                .find(|d| d.id == id)
                .ok_or_else(|| {
                    DispatchError::internal(format!(
                        "dependency {id} was created but could not be read back"
                    ))
                })?;
            to_json(dependency)
        }

        // Procure-to-pay
        "list_suppliers" => to_json(list_suppliers(conn)?),
        "create_supplier" => {
            let name = req_str(args, "name")?;
            let id = create_supplier(conn, &name)?;
            let supplier = list_suppliers(conn)?
                .into_iter()
                .find(|s| s.id == id)
                .ok_or_else(|| {
                    DispatchError::internal(format!(
                        "supplier {id} was created but could not be read back"
                    ))
                })?;
            to_json(supplier)
        }
        "create_requisition" => {
            let new: NewRequisition = parse_as(args, "create_requisition")?;
            let id = create_requisition(conn, &new)?;
            match get_requisition(conn, id)? {
                Some(req) => to_json(req),
                None => Err(DispatchError::internal(format!(
                    "requisition {id} was created but could not be read back"
                ))),
            }
        }
        "list_requisitions" => to_json(list_requisitions(conn)?),
        "get_requisition" => {
            let id = req_i64(args, "requisition_id")?;
            match get_requisition(conn, id)? {
                Some(req) => to_json(req),
                None => Err(DispatchError::refused(format!(
                    "no purchase requisition with id {id}"
                ))),
            }
        }
        "approve_requisition" => requisition_step(conn, args, approve_requisition),
        "reject_requisition" => requisition_step(conn, args, reject_requisition),
        "cancel_requisition" => requisition_step(conn, args, cancel_requisition),
        "create_purchase_order" => {
            let new: NewProcurementOrder = parse_as(args, "create_purchase_order")?;
            let id = create_purchase_order(conn, &new)?;
            match get_procurement_order(conn, id)? {
                Some(order) => to_json(order),
                None => Err(DispatchError::internal(format!(
                    "procurement order {id} was created but could not be read back"
                ))),
            }
        }
        "list_procurement_orders" => to_json(list_procurement_orders(conn)?),
        "get_procurement_order" => {
            let id = req_i64(args, "order_id")?;
            match get_procurement_order(conn, id)? {
                Some(order) => to_json(order),
                None => Err(DispatchError::refused(format!(
                    "no procurement order with id {id}"
                ))),
            }
        }
        "cancel_purchase_order" => {
            let id = req_i64(args, "order_id")?;
            cancel_purchase_order(conn, id)?;
            match get_procurement_order(conn, id)? {
                Some(order) => to_json(order),
                None => Err(DispatchError::internal(format!(
                    "procurement order {id} vanished after cancelling"
                ))),
            }
        }
        "receive_goods" => {
            let po_id = req_i64(args, "order_id")?;
            let lines_value = args.get("lines").cloned().ok_or_else(|| {
                DispatchError::refused(
                    "argument 'lines' is required: an array of {po_line_id, qty}",
                )
            })?;
            let lines: Vec<ReceiveLineInput> = serde_json::from_value(lines_value)
                .map_err(|e| DispatchError::refused(format!("invalid receive_goods lines: {e}")))?;
            let receipt_id = receive_goods(conn, po_id, &lines)?;
            let receipt = list_goods_receipts(conn, Some(po_id))?
                .into_iter()
                .find(|r| r.id == receipt_id)
                .ok_or_else(|| {
                    DispatchError::internal(format!(
                        "goods receipt {receipt_id} was recorded but could not be read back"
                    ))
                })?;
            let entry = match receipt.gl_entry_id {
                Some(_) => entry_or_internal(conn, receipt.gl_entry_id, "goods receipt")?,
                None => Value::Null,
            };
            Ok(json!({
                "receipt": serde_json::to_value(&receipt).unwrap_or(Value::Null),
                "entry": entry,
            }))
        }
        "list_goods_receipts" => to_json(list_goods_receipts(conn, opt_i64(args, "order_id"))?),
        "record_ap_invoice" => {
            let po_id = req_i64(args, "order_id")?;
            let lines_value = args.get("lines").cloned().ok_or_else(|| {
                DispatchError::refused(
                    "argument 'lines' is required: an array of {po_line_id, qty, unit_price_cents}",
                )
            })?;
            let lines: Vec<ApLineInput> = serde_json::from_value(lines_value).map_err(|e| {
                DispatchError::refused(format!("invalid record_ap_invoice lines: {e}"))
            })?;
            let supplier_ref = opt_str(args, "supplier_ref").map(|s| s.to_string());
            let id = record_ap_invoice(
                conn,
                &NewApInvoice {
                    po_id,
                    lines,
                    supplier_ref,
                },
            )?;
            let invoice = get_ap_invoice(conn, id)?.ok_or_else(|| {
                DispatchError::internal(format!(
                    "AP invoice {id} was recorded but could not be read back"
                ))
            })?;
            let entry = entry_or_internal(conn, invoice.gl_entry_id, "AP invoice")?;
            Ok(json!({
                "ap_invoice": serde_json::to_value(&invoice).unwrap_or(Value::Null),
                "entry": entry,
            }))
        }
        "list_ap_invoices" => to_json(list_ap_invoices(conn)?),
        "get_ap_invoice" => {
            let id = req_i64(args, "ap_invoice_id")?;
            match get_ap_invoice(conn, id)? {
                Some(invoice) => to_json(invoice),
                None => Err(DispatchError::refused(format!(
                    "no AP invoice with id {id}"
                ))),
            }
        }
        "override_ap_match" => {
            let id = req_i64(args, "ap_invoice_id")?;
            let reason = req_str(args, "reason")?;
            let by = req_str(args, "by")?;
            override_ap_match(conn, id, &reason, &by)?;
            match get_ap_invoice(conn, id)? {
                Some(invoice) => to_json(invoice),
                None => Err(DispatchError::internal(format!(
                    "AP invoice {id} vanished after the match override"
                ))),
            }
        }
        "pay_ap_invoice" => {
            let ap_invoice_id = req_i64(args, "ap_invoice_id")?;
            let amount = req_i64(args, "amount_cents")?;
            let actor = opt_str(args, "actor");
            let approver = opt_str(args, "approver");
            let payment_id =
                pay_ap_invoice_attributed(conn, ap_invoice_id, amount, actor, approver)?;
            let payment = list_ap_payments_for_invoice(conn, ap_invoice_id)?
                .into_iter()
                .find(|p| p.id == payment_id)
                .ok_or_else(|| {
                    DispatchError::internal(format!(
                        "AP payment {payment_id} was recorded but could not be read back"
                    ))
                })?;
            let entry = entry_or_internal(conn, payment.gl_entry_id, "AP payment")?;
            Ok(json!({
                "payment": serde_json::to_value(&payment).unwrap_or(Value::Null),
                "entry": entry,
            }))
        }
        "list_ap_payments" => match opt_i64(args, "ap_invoice_id") {
            Some(id) => to_json(list_ap_payments_for_invoice(conn, id)?),
            None => to_json(list_ap_payments(conn)?),
        },

        // Controls
        "list_fiscal_periods" => to_json(list_fiscal_periods(conn)?),
        "create_fiscal_period" => {
            let name = req_str(args, "name")?;
            let start = req_str(args, "start_date")?;
            let end = req_str(args, "end_date")?;
            let id = create_fiscal_period(conn, &name, &start, &end)?;
            let period = list_fiscal_periods(conn)?
                .into_iter()
                .find(|p| p.id == id)
                .ok_or_else(|| {
                    DispatchError::internal(format!(
                        "fiscal period {id} was created but could not be read back"
                    ))
                })?;
            to_json(period)
        }
        "close_fiscal_period" => {
            let id = req_i64(args, "period_id")?;
            close_fiscal_period(conn, id)?;
            let period = list_fiscal_periods(conn)?
                .into_iter()
                .find(|p| p.id == id)
                .ok_or_else(|| {
                    DispatchError::internal(format!("fiscal period {id} vanished after closing"))
                })?;
            to_json(period)
        }
        "list_users" => to_json(list_users(conn)?),
        "create_user" => {
            let name = req_str(args, "name")?;
            let role = req_str(args, "role")?;
            let id = create_user(conn, &name, &role)?;
            let user = list_users(conn)?
                .into_iter()
                .find(|u| u.id == id)
                .ok_or_else(|| {
                    DispatchError::internal(format!(
                        "user {id} was created but could not be read back"
                    ))
                })?;
            to_json(user)
        }
        "list_control_settings" => to_json(list_control_settings(conn)?),
        "set_approval_threshold" => {
            let threshold = req_i64(args, "threshold_cents")?;
            set_approval_threshold(conn, threshold)?;
            to_json(list_control_settings(conn)?)
        }

        other => {
            if let Some((_, memo, debit, credit)) = BUILDERS.iter().find(|(n, _, _, _)| *n == other)
            {
                cmd_builder(conn, door, memo, debit, credit, args)
            } else {
                Err(DispatchError::refused(format!("unknown command '{other}'")))
            }
        }
    }
}

fn cmd_health(conn: &Connection) -> DispatchResult {
    // The same counts, in the same shape, as GET /health.
    Ok(json!({
        "status": "ok",
        "accounts": account_count(conn)?,
        "journal_entries": entry_count(conn)?,
        "customers": customer_count(conn)?,
        "quotes": quote_count(conn)?,
        "orders": order_count(conn)?,
        "invoices": invoice_count(conn)?,
        "payments": payment_count(conn)?,
        "config_models": config_model_count(conn)?,
    }))
}

fn quote_transition(
    conn: &mut Connection,
    args: &Value,
    step: fn(&mut Connection, i64) -> Result<(), LedgerError>,
) -> DispatchResult {
    let id = req_i64(args, "quote_id")?;
    step(conn, id)?;
    match get_quote(conn, id)? {
        Some(quote) => to_json(quote),
        None => Err(DispatchError::internal(format!(
            "quote {id} vanished after the transition"
        ))),
    }
}

fn order_transition(
    conn: &mut Connection,
    args: &Value,
    step: fn(&mut Connection, i64) -> Result<(), LedgerError>,
) -> DispatchResult {
    let id = req_i64(args, "order_id")?;
    step(conn, id)?;
    match get_order(conn, id)? {
        Some(order) => to_json(order),
        None => Err(DispatchError::internal(format!(
            "sales order {id} vanished after the transition"
        ))),
    }
}

/// Requisition lifecycle steps take `&Connection` in core (they post
/// nothing), unlike the O2C transitions — hence their own helper.
fn requisition_step(
    conn: &Connection,
    args: &Value,
    step: fn(&Connection, i64) -> Result<(), LedgerError>,
) -> DispatchResult {
    let id = req_i64(args, "requisition_id")?;
    step(conn, id)?;
    match get_requisition(conn, id)? {
        Some(req) => to_json(req),
        None => Err(DispatchError::internal(format!(
            "requisition {id} vanished after the transition"
        ))),
    }
}

/// Read back a posted entry a document transition recorded on its
/// header. A missing id here means the engine and the read-back
/// disagree — an internal failure, never a silent null.
fn entry_or_internal(
    conn: &Connection,
    entry_id: Option<i64>,
    what: &str,
) -> Result<Value, DispatchError> {
    let id = entry_id
        .ok_or_else(|| DispatchError::internal(format!("the {what} entry id was not recorded")))?;
    match get_entry(conn, id)? {
        Some(entry) => serde_json::to_value(entry)
            .map_err(|e| DispatchError::internal(format!("could not serialize entry: {e}"))),
        None => Err(DispatchError::internal(format!(
            "the {what} entry {id} could not be read back"
        ))),
    }
}

/// The builder table read by other layers: (memo, debit account,
/// credit account) for a builder name. The Day-5 agent renders its
/// confirmations from these pairs — the table stays the one source,
/// so a rendered proposal can never promise a different posting than
/// the builder will make.
pub fn builder_pair(name: &str) -> Option<(&'static str, &'static str, &'static str)> {
    BUILDERS
        .iter()
        .find(|(n, _, _, _)| *n == name)
        .map(|(_, memo, debit, credit)| (*memo, *debit, *credit))
}

/// One manual-journal builder: exactly one two-line entry through the
/// one `post_entry`, tagged with the door's source.
fn cmd_builder(
    conn: &mut Connection,
    door: Door,
    memo: &str,
    debit: &str,
    credit: &str,
    args: &Value,
) -> DispatchResult {
    let amount = req_i64(args, "amount_cents")?;
    // Segregation of duties (audit fix, Oct 31): a builder that pays
    // cash OUT is a payment whichever door asks — above the approval
    // threshold it needs an actor and a different approver, exactly
    // like the document payments, and the approver joins the memo.
    // Before this check, the generic pay_supplier builder posted
    // straight through post_entry and bypassed the rule entirely.
    let approved_by = if credit == "1000" {
        erpcraft_core::check_cash_out_approval(
            conn,
            amount,
            opt_str(args, "actor"),
            opt_str(args, "approver"),
        )?
    } else {
        None
    };
    let entry_date = opt_str(args, "entry_date")
        .map(|s| s.to_string())
        .unwrap_or_else(today_utc);
    // The agent door carries the audit memo (intent summary + the
    // verbatim prompt, docs/DAY5_PREP.md); the operator doors keep the
    // builder's fixed memo, so a CLI/MCP caller cannot rewrite what
    // a builder entry says it is.
    let memo = if door == Door::Agent {
        opt_str(args, "memo").unwrap_or(memo).to_string()
    } else {
        memo.to_string()
    };
    let memo = match &approved_by {
        Some(name) => format!("{memo} — approved by {name}"),
        None => memo,
    };
    let new = NewEntry {
        entry_date,
        memo,
        source: door.source_tag().to_string(),
        reverses_entry_id: None,
        lines: vec![
            erpcraft_core::NewLine {
                account_code: debit.to_string(),
                debit_cents: amount,
                credit_cents: 0,
            },
            erpcraft_core::NewLine {
                account_code: credit.to_string(),
                debit_cents: 0,
                credit_cents: amount,
            },
        ],
    };
    let id = post_entry(conn, &new)?;
    match get_entry(conn, id)? {
        Some(entry) => to_json(entry),
        None => Err(DispatchError::internal(format!(
            "entry {id} posted but could not be read back"
        ))),
    }
}

/// Evaluation is a pure read, like over HTTP — except that an
/// *inconsistent* verdict is a refusal at this door (the prep's exit
/// test 4): the error carries the first explanation as its message and
/// the full structured explanations + evaluation in its payload, so
/// nothing about the reasons is lost. Consistent and incomplete
/// verdicts are successful results.
fn cmd_evaluate_config(conn: &Connection, args: &Value) -> DispatchResult {
    let model_id = req_i64(args, "model_id")?;
    let config_value = args.get("configuration").cloned().ok_or_else(|| {
        DispatchError::refused(
            "argument 'configuration' is required: an object mapping characteristic codes to values",
        )
    })?;
    let config: BTreeMap<String, ConfigValue> = serde_json::from_value(config_value)
        .map_err(|e| DispatchError::refused(format!("invalid configuration: {e}")))?;
    let evaluation = evaluate_configuration(conn, model_id, &config)?;
    if matches!(evaluation.verdict, erpcraft_core::Verdict::Inconsistent) {
        let detail = evaluation
            .explanations
            .first()
            .map(|e| e.message_parts.detail.clone())
            .unwrap_or_else(|| "no explanation recorded".to_string());
        return Err(DispatchError {
            message: format!("the configuration is inconsistent: {detail}"),
            explanations: evaluation.explanations.clone(),
            evaluation: Some(Box::new(evaluation)),
            internal: false,
        });
    }
    to_json(evaluation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use erpcraft_core::entry_count;

    fn memory_db() -> Connection {
        open_db(":memory:").unwrap()
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), "1970-01-01");
        assert_eq!(civil_from_days(19_723), "2024-01-01");
        assert_eq!(civil_from_days(20_755), "2026-10-29");
    }

    #[test]
    fn every_spec_has_a_name_usage_and_object_schema() {
        let specs = all_specs();
        assert!(specs.len() >= 40, "the Day-4 table is complete");
        let mut names: Vec<&str> = specs.iter().map(|s| s.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), specs.len(), "command names are unique");
        for spec in &specs {
            assert!(!spec.usage.is_empty(), "{} has a usage line", spec.name);
            assert_eq!(
                spec.input_schema["type"], "object",
                "{} schema is an object",
                spec.name
            );
        }
    }

    #[test]
    fn builders_post_the_preps_account_pairs_tagged_by_door() {
        // The exact pairs from docs/DAY4_PREP.md — a typo here mis-posts
        // real money, so the table is asserted, not trusted.
        let expected: &[(&str, &str, &str, &str)] = &[
            ("buy_materials", "Buy materials", "1200", "2000"),
            ("pay_supplier", "Pay supplier", "2000", "1000"),
            (
                "issue_materials",
                "Issue materials to production",
                "1210",
                "1200",
            ),
            ("record_labor", "Record direct labor", "1210", "1000"),
            ("complete_production", "Complete production", "1220", "1210"),
            ("record_deposit", "Record customer deposit", "1000", "2200"),
            ("record_rent", "Record rent", "6000", "1000"),
            ("record_interest", "Record interest", "6010", "1000"),
            ("record_depreciation", "Record depreciation", "6020", "1310"),
        ];
        assert_eq!(BUILDERS.len(), expected.len());
        for (door, tag) in [(Door::Cli, "cli"), (Door::Mcp, "mcp")] {
            let mut conn = memory_db();
            for (name, memo, debit, credit) in expected {
                let before = entry_count(&conn).unwrap();
                let entry = dispatch(
                    &mut conn,
                    door,
                    name,
                    &json!({ "amount_cents": 12_345, "entry_date": "2026-10-29" }),
                )
                .unwrap_or_else(|e| panic!("{name} refused: {}", e.message));
                assert_eq!(entry["memo"], *memo);
                assert_eq!(entry["source"], tag, "{name} is tagged by its door");
                assert_eq!(entry["lines"][0]["account_code"], *debit);
                assert_eq!(entry["lines"][0]["debit_cents"], 12_345);
                assert_eq!(entry["lines"][1]["account_code"], *credit);
                assert_eq!(entry["lines"][1]["credit_cents"], 12_345);
                assert_eq!(entry_count(&conn).unwrap(), before + 1);
            }
        }
    }

    #[test]
    fn unknown_command_and_missing_args_are_plain_refusals() {
        let mut conn = memory_db();
        let err = dispatch(&mut conn, Door::Cli, "nonsense", &json!({})).unwrap_err();
        assert!(!err.internal);
        assert!(err.message.contains("unknown command 'nonsense'"));

        let err = dispatch(&mut conn, Door::Cli, "issue_invoice", &json!({})).unwrap_err();
        assert!(!err.internal);
        assert!(err.message.contains("'invoice_id'"));
    }

    #[test]
    fn an_unbalanced_post_entry_is_a_refusal_not_an_internal_error() {
        // The DB trigger rejects it; through the door that is the
        // caller's entry being refused (exit 1), not the tool breaking.
        let mut conn = memory_db();
        let err = dispatch(
            &mut conn,
            Door::Cli,
            "post_entry",
            &json!({
                "entry_date": "2026-10-29",
                "memo": "unbalanced",
                "lines": [
                    { "account_code": "1000", "debit_cents": 100 },
                    { "account_code": "4000", "credit_cents": 99 }
                ]
            }),
        )
        .unwrap_err();
        assert!(!err.internal, "message was: {}", err.message);
        assert_eq!(entry_count(&conn).unwrap(), 1, "nothing posted");
    }

    #[test]
    fn post_entry_caller_cannot_forge_the_source_tag() {
        let mut conn = memory_db();
        let entry = dispatch(
            &mut conn,
            Door::Mcp,
            "post_entry",
            &json!({
                "entry_date": "2026-10-29",
                "memo": "forged source attempt",
                "source": "agent",
                "lines": [
                    { "account_code": "1000", "debit_cents": 100 },
                    { "account_code": "4000", "credit_cents": 100 }
                ]
            }),
        )
        .unwrap();
        assert_eq!(
            entry["source"], "mcp",
            "the door sets the tag, not the caller"
        );
    }
}
