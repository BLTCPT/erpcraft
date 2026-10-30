//! The one dispatch table behind both Day-4 doors (DAY4_PREP.md).
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
    accept_quote, account_count, account_ledger, cancel_order, config_model_count,
    create_invoice_from_order, create_order_from_quote, create_quote, customer_count,
    decline_quote, entry_count, evaluate_configuration, expire_quote, fulfil_order,
    get_config_model, get_entry, get_invoice, get_order, get_quote, invoice_count, issue_invoice,
    list_config_models, list_customers, list_entries, list_invoices, list_items, list_orders,
    list_payments, list_payments_for_invoice, list_quotes, open_db, order_count, payment_count,
    post_entry, quote_count, record_payment, record_sales_return, send_quote, trial_balance,
    ConfigValue, Evaluation, Explanation, LedgerError, NewEntry, NewQuote, ReturnLine,
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
}

impl Door {
    pub fn source_tag(self) -> &'static str {
        match self {
            Door::Cli => "cli",
            Door::Mcp => "mcp",
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
            summary: "Record a payment against an issued invoice; posts Dr Cash / Cr AR and advances the invoice toward paid. Overpayment is refused. Returns the payment and its posted entry.",
            usage: "erpcraft record_payment --invoice-id <id> --amount-cents <cents>",
            primary_arg: None,
            input_schema: schema(
                json!({
                    "invoice_id": id_prop("Invoice id"),
                    "amount_cents": { "type": "integer", "description": "Payment amount in cents (positive)" }
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

/// The manual-journal builders (DAY4_PREP.md): the agent-contract
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

/// The full dispatch table, in door order: reads, order-to-cash,
/// manual journal, builders, configurator.
pub fn all_specs() -> Vec<CommandSpec> {
    let mut out = specs();
    for (name, _, _, _) in BUILDERS {
        out.push(builder_spec(name));
    }
    out.extend(configurator_specs());
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
    open_db(&db_path).map_err(|e| {
        DispatchError::internal(format!("cannot open the ledger database at {db_path}: {e}"))
    })
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
            let payment_id = record_payment(conn, invoice_id, amount)?;
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
    let entry_date = opt_str(args, "entry_date")
        .map(|s| s.to_string())
        .unwrap_or_else(today_utc);
    let new = NewEntry {
        entry_date,
        memo: memo.to_string(),
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
        // The exact pairs from DAY4_PREP.md — a typo here mis-posts
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
