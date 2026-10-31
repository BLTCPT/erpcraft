//! The Day-5 natural-language agent (docs/DAY5_PREP.md): a thin adapter
//! over the Day-4 dispatch table.
//!
//! Flow, for every prompt: the model client parses the prompt into a
//! structured intent; the adapter resolves it against the ledger
//! (customers, items, invoices), scans for duplicates, and renders a
//! confirmation **from the proposed dispatch calls and core data** —
//! never from model prose. Nothing posts until a human confirms;
//! [`Proposal::confirm`] is the only path from a proposal to a
//! posting, and it runs through the same `dispatch` the CLI and MCP
//! doors use, tagged `Door::Agent` so agent postings carry
//! `source: "agent"` and a memo with the intent summary and the
//! verbatim prompt (spec rule 8; the Day-5 audit decision in the
//! prep). The agent never writes SQL and never computes its own
//! balances — prices and costs come from the item master and the
//! document line snapshots, exactly as the doors present them.

pub mod clients;

use crate::commands::{builder_pair, dispatch, DispatchError, Door};
use clients::ModelClient;
use erpcraft_core::rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum AgentError {
    /// The model answered, but its reply could not be used.
    Model(String),
    /// No model route could answer (API down, no recording). The
    /// demo continues through the recorded responses or the CLI —
    /// never a crash, never a silent post (spec test 12).
    ModelUnavailable(String),
    /// A dispatch failure that is ours (internal), surfaced as-is.
    Dispatch(DispatchError),
    /// Plumbing: a core refusal during proposal setup. `propose`
    /// converts it into a [`ProposeOutcome::Refusal`] carrying the
    /// core's own plain-language message; it never escapes the
    /// adapter as an error.
    CoreRefusal(String),
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::Model(m) | AgentError::ModelUnavailable(m) | AgentError::CoreRefusal(m) => {
                write!(f, "{m}")
            }
            AgentError::Dispatch(e) => write!(f, "{}", e.message),
        }
    }
}

impl std::error::Error for AgentError {}

// ---------------------------------------------------------------------------
// Parse request — the context the adapter hands the model client.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ParseItem {
    pub id: i64,
    pub sku: String,
    pub name: String,
    pub unit_price_cents: i64,
    pub standard_cost_cents: i64,
}

#[derive(Debug, Clone)]
pub struct ParseInvoice {
    pub id: i64,
    pub customer_name: String,
    pub total_cents: i64,
    pub balance_cents: i64,
}

/// A procurement order still open for receiving (P2P context).
#[derive(Debug, Clone)]
pub struct ParsePo {
    pub id: i64,
    pub supplier_name: String,
    pub total_cents: i64,
    pub received_cents: i64,
}

/// A supplier invoice still carrying a balance (P2P context) —
/// `status` includes `on_hold`, so the model can see a match hold
/// exists before it ever proposes a payment.
#[derive(Debug, Clone)]
pub struct ParseApInvoice {
    pub id: i64,
    pub supplier_name: String,
    pub total_cents: i64,
    pub balance_cents: i64,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct ParseRequest {
    pub prompt: String,
    pub today: String,
    pub customers: Vec<String>,
    pub items: Vec<ParseItem>,
    pub open_invoices: Vec<ParseInvoice>,
    pub suppliers: Vec<String>,
    pub open_purchase_orders: Vec<ParsePo>,
    pub open_ap_invoices: Vec<ParseApInvoice>,
}

// ---------------------------------------------------------------------------
// Outcomes and the proposal
// ---------------------------------------------------------------------------

/// What a prompt came to. Only `NeedsConfirmation` can ever post,
/// and only through [`Proposal::confirm`].
#[derive(Debug)]
pub enum ProposeOutcome {
    NeedsConfirmation(Box<Proposal>),
    /// A question the human must answer before anything is proposed
    /// (missing quantity, a near-miss customer name, several open
    /// invoices). Nothing was created and nothing posted.
    Clarification {
        prompt: String,
        question: String,
    },
    /// A plain-language no, with the reason (unknown customer, no
    /// open invoice, a price the chain cannot post). Nothing posted.
    Refusal {
        prompt: String,
        message: String,
    },
    /// Outside the agent's twelve intents; the message names what
    /// the agent *can* do instead of guessing a posting.
    OutOfScope {
        prompt: String,
        message: String,
    },
    /// The prompt restated a price that conflicts with the price
    /// list. Nothing is proposed until the human picks one
    /// ([`ProposeOptions::price_choice`]).
    PriceConflict {
        prompt: String,
        question: String,
    },
    /// The proposed posting matches one already posted. It is only
    /// ever proposed after an explicit override
    /// ([`ProposeOptions::allow_duplicate`]).
    DuplicateWarning {
        prompt: String,
        warning: String,
    },
    /// A read-only answer (an MRP check): computed live from the
    /// ledger, nothing created, nothing proposed, nothing posted.
    Answer {
        prompt: String,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceChoice {
    /// Post at the price-list price.
    ListPrice,
    /// The human insists on the prompt's one-off price.
    OneOff,
}

#[derive(Debug, Clone, Default)]
pub struct ProposeOptions {
    /// The human has seen the duplicate warning and said "post it
    /// anyway" (spec rule 6).
    pub allow_duplicate: bool,
    /// The human has answered a price conflict (spec test 11).
    pub price_choice: Option<PriceChoice>,
}

/// A confirmed-ready proposal: the rendered confirmation and the one
/// gated dispatch call that posts it. For the invoice intent the
/// pre-gate chain (quote → order → draft invoice) has already run —
/// all draft state — and the gate call is `issue_invoice`, the only
/// step that posts (docs/DAY5_PREP.md).
#[derive(Debug)]
pub struct Proposal {
    pub prompt: String,
    pub intent: String,
    pub summary: String,
    pub confirmation_text: String,
    /// The duplicate warning, when the human overrode one to get
    /// this proposal; it is printed at the top of the confirmation.
    pub duplicate_warning: Option<String>,
    gate: GateCall,
    draft_invoice_id: Option<i64>,
}

#[derive(Debug)]
enum GateCall {
    IssueInvoice(i64),
    Dispatch {
        command: String,
        args: Value,
    },
    /// Several dispatch calls authorized by the one confirmation,
    /// run in order (an AP match override immediately followed by
    /// the payment it releases). The human sees every step in the
    /// rendered confirmation before confirming any of them.
    Sequence(Vec<(String, Value)>),
}

impl Proposal {
    /// A stable fingerprint of the gated action (not the wording):
    /// the same calls with the same arguments are the same action.
    fn fingerprint(&self) -> String {
        use std::hash::{Hash, Hasher};
        let canonical = match &self.gate {
            GateCall::IssueInvoice(id) => format!("issue_invoice:{id}"),
            GateCall::Dispatch { command, args } => format!("{command}:{args}"),
            GateCall::Sequence(calls) => calls
                .iter()
                .map(|(c, a)| format!("{c}:{a}"))
                .collect::<Vec<_>>()
                .join("|"),
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        canonical.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    /// Validate every posting step of a multi-step action BEFORE the
    /// first step runs (audit fix, Oct 31): the only sequences the
    /// agent builds are AP match-override + payment, and a refused
    /// payment must never leave the override behind it.
    fn prevalidate_sequence(
        &self,
        conn: &Connection,
        calls: &[(String, Value)],
    ) -> Result<(), AgentError> {
        let mut overridden: Vec<i64> = Vec::new();
        for (command, args) in calls {
            match command.as_str() {
                "override_ap_match" => overridden.push(j_i64(args, "ap_invoice_id")),
                "pay_ap_invoice" => {
                    let id = j_i64(args, "ap_invoice_id");
                    erpcraft_core::check_ap_payment_ready(
                        conn,
                        id,
                        j_i64(args, "amount_cents"),
                        overridden.contains(&id),
                    )
                    .map_err(AgentError::CoreRefusal)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Confirm-time revalidation for invoice issuance (audit fix,
    /// Oct 31): the world may have changed since the proposal was
    /// rendered — in particular, a second proposal for the same
    /// shipment may have confirmed first. If an issued invoice for
    /// the same customer and total already exists (other than this
    /// draft), issuing is a double-post: refuse.
    fn check_not_already_invoiced(
        &self,
        conn: &mut Connection,
        invoice_id: i64,
    ) -> Result<(), AgentError> {
        // A proposal the human explicitly pushed past a duplicate
        // warning (allow_duplicate) is a decision, not a replay —
        // the warning was shown and overridden before this gate.
        if self.duplicate_warning.is_some() {
            return Ok(());
        }
        let draft = dispatch(conn, Door::Agent, "get_invoice", &json!({ "invoice_id": invoice_id }))
            .map_err(AgentError::Dispatch)?;
        let customer_id = j_i64(&draft, "customer_id");
        let total = j_i64(&draft, "total_cents");
        let invoices = dispatch(conn, Door::Agent, "list_invoices", &json!({}))
            .map_err(AgentError::Dispatch)?;
        let clash = invoices.as_array().and_then(|rows| {
            rows.iter().find(|inv| {
                j_i64(inv, "id") != invoice_id
                    && j_i64(inv, "customer_id") == customer_id
                    && j_i64(inv, "total_cents") == total
                    && matches!(
                        j_str(inv, "status"),
                        "issued" | "partially_paid" | "paid"
                    )
            })
        });
        if let Some(existing) = clash {
            return Err(AgentError::CoreRefusal(format!(
                "invoice #{} for the same customer and total ({}) was already issued; \
                 refusing to issue this shipment twice",
                j_i64(existing, "id"),
                fmt_cents(total)
            )));
        }
        Ok(())
    }

    /// The confirm half of the gate: run the posting call(s)
    /// through the dispatch table, tagged as the agent door.
    pub fn confirm(&self, conn: &mut Connection) -> Result<Value, AgentError> {
        // One confirmation, one posting (audit fix, Oct 31): a
        // confirmed action's fingerprint is logged, and confirming
        // the same action again — this proposal twice, or a second
        // proposal carrying the identical gated call — is refused.
        // A confirmation authorizes one posting, not a replay token.
        let fingerprint = self.fingerprint();
        if erpcraft_core::agent_action_confirmed(conn, &fingerprint)
            .map_err(|e| AgentError::CoreRefusal(e.to_string()))?
        {
            return Err(AgentError::CoreRefusal(format!(
                "this action was already confirmed and posted ({}); refusing to post it twice",
                self.summary
            )));
        }
        if let GateCall::Sequence(calls) = &self.gate {
            self.prevalidate_sequence(conn, calls)?;
        }
        if let GateCall::IssueInvoice(invoice_id) = &self.gate {
            self.check_not_already_invoiced(conn, *invoice_id)?;
        }
        let result = match &self.gate {
            GateCall::IssueInvoice(invoice_id) => dispatch(
                conn,
                Door::Agent,
                "issue_invoice",
                &json!({ "invoice_id": invoice_id }),
            ),
            GateCall::Dispatch { command, args } => dispatch(conn, Door::Agent, command, args),
            GateCall::Sequence(calls) => {
                let mut combined = serde_json::Map::new();
                for (command, args) in calls {
                    let step =
                        dispatch(conn, Door::Agent, command, args).map_err(AgentError::Dispatch)?;
                    combined.insert(command.clone(), step);
                }
                Ok(Value::Object(combined))
            }
        };
        let value = result.map_err(AgentError::Dispatch)?;
        erpcraft_core::record_agent_action(conn, &fingerprint, &self.summary)
            .map_err(|e| AgentError::CoreRefusal(e.to_string()))?;
        Ok(value)
    }

    /// The cancel half: nothing has posted, so there is nothing to
    /// reverse. An intent-1 draft invoice simply stays unissued.
    pub fn cancel_message(&self) -> String {
        match self.draft_invoice_id {
            Some(id) => format!("Cancelled — nothing posted. Draft invoice #{id} stays unissued."),
            None => "Cancelled — nothing posted.".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// The agent
// ---------------------------------------------------------------------------

/// The ledger context one proposal is resolved against: the reads
/// the adapter gathered (through the dispatch table) before the
/// model was called.
struct Ctx<'a> {
    customers: &'a [(i64, String)],
    items: &'a [ParseItem],
    invoices: &'a [Value],
}

pub struct Agent<C: ModelClient> {
    client: C,
    today: String,
}

impl<C: ModelClient> Agent<C> {
    /// `today` is the demo day (`YYYY-MM-DD`) relative days resolve
    /// against — injected so the acceptance suite is deterministic.
    pub fn new(client: C, today: impl Into<String>) -> Self {
        Agent {
            client,
            today: today.into(),
        }
    }

    pub fn propose(
        &self,
        conn: &mut Connection,
        prompt: &str,
    ) -> Result<ProposeOutcome, AgentError> {
        self.propose_with(conn, prompt, &ProposeOptions::default())
    }

    pub fn propose_with(
        &self,
        conn: &mut Connection,
        prompt: &str,
        options: &ProposeOptions,
    ) -> Result<ProposeOutcome, AgentError> {
        match self.propose_inner(conn, prompt, options) {
            Ok(outcome) => Ok(outcome),
            Err(AgentError::CoreRefusal(message)) => Ok(ProposeOutcome::Refusal {
                prompt: prompt.to_string(),
                message,
            }),
            Err(other) => Err(other),
        }
    }

    fn propose_inner(
        &self,
        conn: &mut Connection,
        prompt: &str,
        options: &ProposeOptions,
    ) -> Result<ProposeOutcome, AgentError> {
        // Context — gathered through the dispatch reads, the same
        // numbers every door sees.
        let customers = self.customers(conn)?;
        let items = self.items(conn)?;
        let invoices = self.call(conn, "list_invoices", &json!({}))?;
        let invoice_rows: Vec<Value> = invoices.as_array().cloned().unwrap_or_default();

        // Procure-to-pay context, the same reads the P2P propose
        // paths resolve against below.
        let supplier_rows = self.call(conn, "list_suppliers", &json!({}))?;
        let po_rows = self.call(conn, "list_procurement_orders", &json!({}))?;
        let ap_rows = self.call(conn, "list_ap_invoices", &json!({}))?;

        let request = ParseRequest {
            prompt: prompt.to_string(),
            today: self.today.clone(),
            customers: customers.iter().map(|(_, n)| n.clone()).collect(),
            items: items.clone(),
            open_invoices: invoice_rows
                .iter()
                .filter(|inv| {
                    matches!(j_str(inv, "status"), "issued" | "partially_paid")
                        && j_i64(inv, "balance_cents") > 0
                })
                .map(|inv| ParseInvoice {
                    id: j_i64(inv, "id"),
                    customer_name: j_str(inv, "customer_name").to_string(),
                    total_cents: j_i64(inv, "total_cents"),
                    balance_cents: j_i64(inv, "balance_cents"),
                })
                .collect(),
            suppliers: supplier_rows
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|s| j_str(s, "name").to_string())
                .collect(),
            open_purchase_orders: po_rows
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter(|po| matches!(j_str(po, "status"), "open" | "partially_received"))
                .map(|po| ParsePo {
                    id: j_i64(po, "id"),
                    supplier_name: j_str(po, "supplier_name").to_string(),
                    total_cents: j_i64(po, "total_cents"),
                    received_cents: j_i64(po, "received_cents"),
                })
                .collect(),
            open_ap_invoices: ap_rows
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter(|inv| {
                    matches!(j_str(inv, "status"), "open" | "partially_paid" | "on_hold")
                        && j_i64(inv, "balance_cents") > 0
                })
                .map(|inv| ParseApInvoice {
                    id: j_i64(inv, "id"),
                    supplier_name: j_str(inv, "supplier_name").to_string(),
                    total_cents: j_i64(inv, "total_cents"),
                    balance_cents: j_i64(inv, "balance_cents"),
                    status: j_str(inv, "status").to_string(),
                })
                .collect(),
        };

        let raw = self.client.parse_intent(&request)?;
        let Some(intent) = parse_intent_json(&raw) else {
            return Ok(ProposeOutcome::Refusal {
                prompt: prompt.to_string(),
                message: "I couldn't turn that into one of the postings I know how to \
                          propose. Try: \"Invoice Acme for the 40 units we shipped Tuesday.\""
                    .to_string(),
            });
        };

        let ctx = Ctx {
            customers: &customers,
            items: &items,
            invoices: &invoice_rows,
        };
        match intent.intent.as_str() {
            "invoice_shipment" => self.propose_invoice(conn, prompt, &intent, &ctx, options),
            "record_payment" => self.propose_payment(conn, prompt, &intent, &ctx, options),
            "record_return" => self.propose_return(conn, prompt, &intent, &ctx, options),
            "buy_materials"
            | "pay_supplier"
            | "issue_materials"
            | "record_labor"
            | "complete_production"
            | "record_deposit"
            | "record_rent"
            | "record_interest"
            | "record_depreciation" => {
                self.propose_builder(conn, prompt, &intent, &customers, options)
            }
            "mrp_check" => self.propose_mrp_check(conn, prompt, &intent, &ctx),
            "mrp_release" => self.propose_mrp_release(conn, prompt, &intent, &ctx),
            "receive_goods" => self.propose_receive_goods(conn, prompt, &intent),
            "pay_ap_invoice" => self.propose_pay_ap_invoice(conn, prompt, &intent, options),
            "out_of_scope" => {
                let topic = intent.topic.as_deref().unwrap_or("that");
                Ok(ProposeOutcome::OutOfScope {
                    prompt: prompt.to_string(),
                    message: format!(
                        "I don't handle {topic} yet. I can invoice shipped units, record \
                         payments, deposits and returns, buy materials on credit, pay \
                         suppliers, issue materials, record labor and production, and \
                         post rent, interest and depreciation. I can also receive goods \
                         against a purchase order, pay supplier (AP) invoices, check \
                         material plans — ask \"What are we short on?\" or \"Can we build \
                         200 BRK-100 by 2026-11-15?\" — and propose releasing a planned order."
                    ),
                })
            }
            _ => Ok(ProposeOutcome::Refusal {
                prompt: prompt.to_string(),
                message: "I couldn't turn that into one of the postings I know how to \
                          propose. Try: \"Invoice Acme for the 40 units we shipped Tuesday.\""
                    .to_string(),
            }),
        }
    }

    // -- Intent 1: invoice a shipment --------------------------------

    fn propose_invoice(
        &self,
        conn: &mut Connection,
        prompt: &str,
        intent: &Intent,
        ctx: &Ctx,
        options: &ProposeOptions,
    ) -> Result<ProposeOutcome, AgentError> {
        let (customers, items, invoices) = (ctx.customers, ctx.items, ctx.invoices);
        let (customer_id, customer_name) =
            match self.require_customer(customers, intent.customer.as_deref(), prompt) {
                Ok(pair) => pair,
                Err(outcome) => return Ok(outcome),
            };
        let Some(qty) = intent.qty.filter(|q| *q > 0) else {
            return Ok(ProposeOutcome::Clarification {
                prompt: prompt.to_string(),
                question: "How many units, and which day?".to_string(),
            });
        };
        let item = match self.require_item(items, prompt) {
            Ok(item) => item,
            Err(outcome) => return Ok(outcome),
        };

        // Rule 2: prices come from the price list, never the prompt.
        let mut price_note: Option<String> = None;
        if let Some(stated) = money_slot(&intent.price_each)? {
            if stated != item.unit_price_cents {
                match options.price_choice {
                    None => {
                        return Ok(ProposeOutcome::PriceConflict {
                            prompt: prompt.to_string(),
                            question: format!(
                                "The price list says {}/unit. Post at {}, or confirm {} as a one-off?",
                                fmt_cents(item.unit_price_cents),
                                fmt_cents(item.unit_price_cents),
                                fmt_cents(stated)
                            ),
                        });
                    }
                    Some(PriceChoice::ListPrice) => {
                        price_note = Some(format!(
                            "Price: the price-list {} (the prompt's {} was not used)",
                            fmt_cents(item.unit_price_cents),
                            fmt_cents(stated)
                        ));
                    }
                    Some(PriceChoice::OneOff) => {
                        return Ok(ProposeOutcome::Refusal {
                            prompt: prompt.to_string(),
                            message: format!(
                                "A one-off price of {} can't be posted through the \
                                 order-to-cash chain — quote lines snapshot the item \
                                 master's {} and no step overrides it. Change the item \
                                 price first, or post at the price-list price.",
                                fmt_cents(stated),
                                fmt_cents(item.unit_price_cents)
                            ),
                        });
                    }
                }
            }
        }

        let total = qty * item.unit_price_cents;

        // Rule 6: duplicates are flagged before any draft is created.
        let duplicate_warning = invoices
            .iter()
            .find(|inv| {
                j_i64(inv, "customer_id") == customer_id
                    && matches!(j_str(inv, "status"), "issued" | "partially_paid" | "paid")
                    && j_i64(inv, "total_cents") == total
            })
            .map(|existing| {
                format!(
                    "This looks like a duplicate of the {customer_name} invoice #{} \
                     already posted ({}).",
                    j_i64(existing, "id"),
                    fmt_cents(total)
                )
            });
        if let Some(warning) = &duplicate_warning {
            if !options.allow_duplicate {
                return Ok(ProposeOutcome::DuplicateWarning {
                    prompt: prompt.to_string(),
                    warning: format!(
                        "{warning} I won't post it twice — say the word and I'll \
                         prepare it anyway."
                    ),
                });
            }
        }

        // The pre-gate chain: draft state only. If a fulfilled order
        // for this customer, total AND quantity already exists and
        // has no invoice yet, start there. A fulfilled order matching
        // only the total is NOT the same shipment (125 configured
        // brackets at $162 total exactly what 162 plain ones at $125
        // do — audit fix, Oct 31): ask, don't silently invoice it.
        let order_id = match self.fulfilled_order_for(conn, customer_id, total, qty, invoices)? {
            Some(id) => id,
            None => {
                if let Some((other_id, other_qty, configured)) =
                    self.fulfilled_qty_mismatch(conn, customer_id, total, qty, invoices)?
                {
                    return Ok(ProposeOutcome::Clarification {
                        prompt: prompt.to_string(),
                        question: format!(
                            "There's a fulfilled order #{other_id} for {customer_name} \
                             totalling {}, but it's {other_qty} {} — not the {qty} units \
                             in your request, and the same total isn't the same shipment. \
                             Invoice order #{other_id} as it stands, or is the {qty}-unit \
                             shipment a separate one?",
                            fmt_cents(total),
                            if configured { "configured brackets" } else { "brackets" },
                        ),
                    });
                }
                let quote = self.call(
                    conn,
                    "create_quote",
                    &json!({
                        "customer_id": customer_id,
                        "lines": [{ "item_id": item.id, "qty": qty }],
                    }),
                )?;
                let quote_id = j_i64(&quote, "id");
                self.call(conn, "send_quote", &json!({ "quote_id": quote_id }))?;
                self.call(conn, "accept_quote", &json!({ "quote_id": quote_id }))?;
                let order = self.call(conn, "create_order", &json!({ "quote_id": quote_id }))?;
                let order_id = j_i64(&order, "id");
                self.call(conn, "fulfil_order", &json!({ "order_id": order_id }))?;
                order_id
            }
        };
        let draft = self.call(conn, "create_invoice", &json!({ "order_id": order_id }))?;
        let invoice_id = j_i64(&draft, "id");

        // Render from the draft invoice — its snapshotted prices and
        // costs are what issuance will post. EVERY rendered number
        // comes from the draft document itself, never from the
        // request: a confirmation that describes a different document
        // than the one being issued is a false confirmation
        // (audit fix, Oct 31).
        let draft_lines: Vec<Value> = draft
            .get("lines")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut cogs: i64 = 0;
        for line in &draft_lines {
            match line.get("standard_cost_cents").and_then(|v| v.as_i64()) {
                Some(cost) => cogs += j_i64(line, "qty") * cost,
                None => {
                    return Ok(ProposeOutcome::Refusal {
                        prompt: prompt.to_string(),
                        message: format!(
                            "The draft invoice's line for {} has no standard-cost \
                             snapshot, so issuance would be refused — nothing can be \
                             proposed until the item carries a standard cost.",
                            j_str(line, "sku")
                        ),
                    })
                }
            }
        }
        let draft_total = j_i64(&draft, "total_cents");
        // Heading, COGS note and summary all describe the DRAFT.
        let (heading, cogs_note, summary_qty) = match draft_lines.len() {
            1 => {
                let l = &draft_lines[0];
                let l_qty = j_i64(l, "qty");
                let l_cost = l
                    .get("standard_cost_cents")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                (
                    format!(
                        "Invoice {customer_name} for {l_qty} brackets @ {} = {}",
                        fmt_cents(j_i64(l, "unit_price_cents")),
                        fmt_cents(draft_total)
                    ),
                    format!("{l_qty} × {} standard cost", fmt_cents(l_cost)),
                    l_qty,
                )
            }
            _ => (
                format!(
                    "Invoice {customer_name} — {} lines totalling {}",
                    draft_lines.len(),
                    fmt_cents(draft_total)
                ),
                "standard costs from the lines' snapshots".to_string(),
                qty,
            ),
        };

        let mut sublines: Vec<String> = Vec::new();
        if let Some(day) = intent.day.as_deref() {
            if let Ok(date) = resolve_day(&self.today, Some(day)) {
                sublines.push(format!("Shipped {day} ({date})"));
            }
        }
        if let Some(note) = price_note {
            sublines.push(note);
        }
        if draft_lines.len() == 1 {
            if let Some(cfg) = draft_lines[0]
                .get("configuration")
                .and_then(|v| v.as_object())
            {
                if !cfg.is_empty() {
                    let parts: Vec<String> = cfg
                        .iter()
                        .map(|(k, v)| {
                            format!("{k}={}", v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string()))
                        })
                        .collect();
                    sublines.push(format!("Configuration: {}", parts.join(", ")));
                }
            }
        }
        let lines = vec![
            ConfirmLine {
                code: "1100".to_string(),
                debit: draft_total,
                credit: 0,
                note: Some(format!("{customer_name} owes us")),
            },
            ConfirmLine {
                code: "4000".to_string(),
                debit: 0,
                credit: draft_total,
                note: None,
            },
            ConfirmLine {
                code: "5000".to_string(),
                debit: cogs,
                credit: 0,
                note: Some(cogs_note),
            },
            ConfirmLine {
                code: "1220".to_string(),
                debit: 0,
                credit: cogs,
                note: None,
            },
        ];
        let confirmation = self.render(conn, &heading, &sublines, &lines)?;
        Ok(ProposeOutcome::NeedsConfirmation(Box::new(Proposal {
            prompt: prompt.to_string(),
            intent: "invoice_shipment".to_string(),
            summary: format!("Invoice {customer_name} for {summary_qty} brackets"),
            confirmation_text: confirmation,
            duplicate_warning,
            gate: GateCall::IssueInvoice(invoice_id),
            draft_invoice_id: Some(invoice_id),
        })))
    }

    // -- Intent 2: a payment against an invoice ----------------------

    fn propose_payment(
        &self,
        conn: &mut Connection,
        prompt: &str,
        intent: &Intent,
        ctx: &Ctx,
        options: &ProposeOptions,
    ) -> Result<ProposeOutcome, AgentError> {
        let (customers, invoices) = (ctx.customers, ctx.invoices);
        let (customer_id, customer_name) =
            match self.require_customer(customers, intent.customer.as_deref(), prompt) {
                Ok(pair) => pair,
                Err(outcome) => return Ok(outcome),
            };
        let amount = match self.amount_slot(intent, prompt) {
            Ok(amount) => amount,
            Err(outcome) => return Ok(outcome),
        };
        let Some(amount) = amount else {
            return Ok(ProposeOutcome::Clarification {
                prompt: prompt.to_string(),
                question: format!("How much did {customer_name} pay?"),
            });
        };

        let open: Vec<&Value> = invoices
            .iter()
            .filter(|inv| {
                j_i64(inv, "customer_id") == customer_id
                    && matches!(j_str(inv, "status"), "issued" | "partially_paid")
                    && j_i64(inv, "balance_cents") > 0
            })
            .collect();
        let invoice = match open.len() {
            0 => {
                return Ok(ProposeOutcome::Refusal {
                    prompt: prompt.to_string(),
                    message: format!("{customer_name} has no open invoice to pay against."),
                })
            }
            1 => open[0],
            _ => {
                let list = open
                    .iter()
                    .map(|inv| {
                        format!(
                            "#{} ({} due)",
                            j_i64(inv, "id"),
                            fmt_cents(j_i64(inv, "balance_cents"))
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                return Ok(ProposeOutcome::Clarification {
                    prompt: prompt.to_string(),
                    question: format!(
                        "{customer_name} has {} open invoices: {list}. Which one did they pay against?",
                        open.len()
                    ),
                });
            }
        };
        let invoice_id = j_i64(invoice, "id");
        let balance = j_i64(invoice, "balance_cents");
        if amount > balance {
            return Ok(ProposeOutcome::Refusal {
                prompt: prompt.to_string(),
                message: format!(
                    "{} is more than the {} balance on invoice #{invoice_id} — overpayment is refused.",
                    fmt_cents(amount),
                    fmt_cents(balance)
                ),
            });
        }

        let payments = self.call(
            conn,
            "list_payments_for_invoice",
            &json!({ "invoice_id": invoice_id }),
        )?;
        let duplicate_warning = payments.as_array().and_then(|rows| {
            rows.iter()
                .find(|p| j_i64(p, "amount_cents") == amount)
                .map(|_| {
                    format!(
                        "A payment of {} against invoice #{invoice_id} is already recorded.",
                        fmt_cents(amount)
                    )
                })
        });
        if let Some(warning) = &duplicate_warning {
            if !options.allow_duplicate {
                return Ok(ProposeOutcome::DuplicateWarning {
                    prompt: prompt.to_string(),
                    warning: format!(
                        "{warning} I won't post it twice — say the word and I'll \
                         prepare it anyway."
                    ),
                });
            }
        }

        let lines = vec![
            ConfirmLine {
                code: "1000".to_string(),
                debit: amount,
                credit: 0,
                note: None,
            },
            ConfirmLine {
                code: "1100".to_string(),
                debit: 0,
                credit: amount,
                note: Some(format!(
                    "{customer_name}'s balance drops to {}",
                    fmt_cents(balance - amount)
                )),
            },
        ];
        let confirmation = self.render(
            conn,
            &format!(
                "{customer_name} paid {} against invoice #{invoice_id}",
                fmt_cents(amount)
            ),
            &[],
            &lines,
        )?;
        Ok(ProposeOutcome::NeedsConfirmation(Box::new(Proposal {
            prompt: prompt.to_string(),
            intent: "record_payment".to_string(),
            summary: format!("Record payment from {customer_name}"),
            confirmation_text: confirmation,
            duplicate_warning,
            gate: GateCall::Dispatch {
                command: "record_payment".to_string(),
                args: json!({ "invoice_id": invoice_id, "amount_cents": amount }),
            },
            draft_invoice_id: None,
        })))
    }

    // -- Intent 9: a return ------------------------------------------

    fn propose_return(
        &self,
        conn: &mut Connection,
        prompt: &str,
        intent: &Intent,
        ctx: &Ctx,
        options: &ProposeOptions,
    ) -> Result<ProposeOutcome, AgentError> {
        let (customers, items, invoices) = (ctx.customers, ctx.items, ctx.invoices);
        let (customer_id, customer_name) =
            match self.require_customer(customers, intent.customer.as_deref(), prompt) {
                Ok(pair) => pair,
                Err(outcome) => return Ok(outcome),
            };
        let item = match self.require_item(items, prompt) {
            Ok(item) => item,
            Err(outcome) => return Ok(outcome),
        };
        let Some(qty) = intent.qty.filter(|q| *q > 0) else {
            return Ok(ProposeOutcome::Clarification {
                prompt: prompt.to_string(),
                question: format!("How many brackets did {customer_name} return?"),
            });
        };

        let candidates: Vec<&Value> = invoices
            .iter()
            .filter(|inv| {
                j_i64(inv, "customer_id") == customer_id
                    && matches!(j_str(inv, "status"), "issued" | "partially_paid" | "paid")
                    && inv
                        .get("lines")
                        .and_then(|v| v.as_array())
                        .map(|lines| lines.iter().any(|l| j_i64(l, "item_id") == item.id))
                        .unwrap_or(false)
            })
            .collect();
        let invoice = match candidates.len() {
            0 => {
                return Ok(ProposeOutcome::Refusal {
                    prompt: prompt.to_string(),
                    message: format!(
                        "There is no issued invoice for {customer_name} with brackets on it to return against."
                    ),
                })
            }
            1 => candidates[0],
            _ => {
                let list = candidates
                    .iter()
                    .map(|inv| format!("#{}", j_i64(inv, "id")))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Ok(ProposeOutcome::Clarification {
                    prompt: prompt.to_string(),
                    question: format!(
                        "{customer_name} has several invoices with brackets ({list}). Which invoice are the {qty} returned brackets from?"
                    ),
                });
            }
        };
        let invoice_id = j_i64(invoice, "id");
        let line = invoice
            .get("lines")
            .and_then(|v| v.as_array())
            .and_then(|lines| {
                lines
                    .iter()
                    .find(|l| j_i64(l, "item_id") == item.id)
                    .cloned()
            })
            .unwrap_or(json!({}));
        let invoiced_qty = j_i64(&line, "qty");
        if qty > invoiced_qty {
            return Ok(ProposeOutcome::Refusal {
                prompt: prompt.to_string(),
                message: format!(
                    "Invoice #{invoice_id} shipped {invoiced_qty} brackets — {qty} can't come back against it."
                ),
            });
        }
        let unit_price = j_i64(&line, "unit_price_cents");
        let Some(unit_cost) = line.get("standard_cost_cents").and_then(|v| v.as_i64()) else {
            return Ok(ProposeOutcome::Refusal {
                prompt: prompt.to_string(),
                message: format!(
                    "Invoice #{invoice_id}'s bracket line has no standard-cost snapshot, so the return's cost reversal can't be computed."
                ),
            });
        };
        let revenue = qty * unit_price;
        let cost = qty * unit_cost;

        let entries = self.call(conn, "list_entries", &json!({}))?;
        let already = entries
            .as_array()
            .map(|rows| {
                rows.iter().any(|e| {
                    j_str(e, "source") == format!("return:{invoice_id}")
                        && e.get("lines")
                            .and_then(|v| v.as_array())
                            .map(|lines| {
                                lines.iter().any(|l| {
                                    j_str(l, "account_code") == "4010"
                                        && j_i64(l, "debit_cents") == revenue
                                })
                            })
                            .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        let duplicate_warning = already.then(|| {
            format!("A return of {qty} brackets against invoice #{invoice_id} is already posted.")
        });
        if let Some(warning) = &duplicate_warning {
            if !options.allow_duplicate {
                return Ok(ProposeOutcome::DuplicateWarning {
                    prompt: prompt.to_string(),
                    warning: format!(
                        "{warning} I won't post it twice — say the word and I'll \
                         prepare it anyway."
                    ),
                });
            }
        }

        let lines = vec![
            ConfirmLine {
                code: "4010".to_string(),
                debit: revenue,
                credit: 0,
                note: Some(format!("{qty} × {} sale price", fmt_cents(unit_price))),
            },
            ConfirmLine {
                code: "1100".to_string(),
                debit: 0,
                credit: revenue,
                note: Some(format!("{customer_name} owes less")),
            },
            ConfirmLine {
                code: "1220".to_string(),
                debit: cost,
                credit: 0,
                note: Some(format!(
                    "{qty} × {} standard cost, back in stock",
                    fmt_cents(unit_cost)
                )),
            },
            ConfirmLine {
                code: "5000".to_string(),
                debit: 0,
                credit: cost,
                note: None,
            },
        ];
        let confirmation = self.render(
            conn,
            &format!("{customer_name} returned {qty} defective brackets"),
            &[format!(
                "Corrects invoice #{invoice_id} — both entries reference the original sale"
            )],
            &lines,
        )?;
        Ok(ProposeOutcome::NeedsConfirmation(Box::new(Proposal {
            prompt: prompt.to_string(),
            intent: "record_return".to_string(),
            summary: format!("Record return from {customer_name}"),
            confirmation_text: confirmation,
            duplicate_warning,
            gate: GateCall::Dispatch {
                command: "record_return".to_string(),
                args: json!({
                    "invoice_id": invoice_id,
                    "lines": [{ "item_id": item.id, "qty": qty }],
                }),
            },
            draft_invoice_id: None,
        })))
    }

    // -- Intents 3–8, 10–12: the manual-journal builders -------------

    fn propose_builder(
        &self,
        conn: &mut Connection,
        prompt: &str,
        intent: &Intent,
        customers: &[(i64, String)],
        options: &ProposeOptions,
    ) -> Result<ProposeOutcome, AgentError> {
        let name = intent.intent.as_str();
        let (_, debit, credit) = builder_pair(name).ok_or_else(|| {
            AgentError::Model(format!(
                "intent '{name}' has no builder in the dispatch table"
            ))
        })?;

        // The deposit names a customer; resolve it for the proposal
        // and the memo (the builder itself takes no customer arg).
        let mut customer_name: Option<String> = None;
        if name == "record_deposit" {
            match self.require_customer(customers, intent.customer.as_deref(), prompt) {
                Ok((_, resolved)) => customer_name = Some(resolved),
                Err(outcome) => return Ok(outcome),
            }
        }

        let amount = match self.amount_slot(intent, prompt) {
            Ok(amount) => amount,
            Err(outcome) => return Ok(outcome),
        };
        let Some(amount) = amount else {
            return Ok(ProposeOutcome::Clarification {
                prompt: prompt.to_string(),
                question: match name {
                    "record_rent" => "How much is the rent?".to_string(),
                    "record_interest" => "How much is the interest?".to_string(),
                    "record_depreciation" => "How much depreciation?".to_string(),
                    "record_deposit" => "How much was the deposit?".to_string(),
                    "complete_production" => {
                        "What did the finished brackets cost in total?".to_string()
                    }
                    _ => "How much?".to_string(),
                },
            });
        };

        let entry_date = match resolve_day(&self.today, intent.day.as_deref()) {
            Ok(date) => date,
            Err(unclear) => {
                return Ok(ProposeOutcome::Clarification {
                    prompt: prompt.to_string(),
                    question: unclear,
                })
            }
        };

        // Cash-out builders are payments (audit fix, Oct 31): above
        // the approval threshold they need an actor and a different
        // approver, which an agent proposal can't carry — refuse up
        // front, exactly like pay_ap_invoice; the dispatch gate
        // enforces the same rule for direct callers.
        if credit == "1000" {
            let settings = self.call(conn, "list_control_settings", &json!({}))?;
            let threshold = settings.as_array().and_then(|rows| {
                rows.iter()
                    .find(|s| j_str(s, "key") == "payment_approval_threshold_cents")
                    .and_then(|s| j_str(s, "value").parse::<i64>().ok())
            });
            if let Some(threshold) = threshold {
                if amount > threshold {
                    return Ok(ProposeOutcome::Refusal {
                        prompt: prompt.to_string(),
                        message: format!(
                            "Paying {} is above the {} payment-approval threshold: the \
                             {name} command must name an actor and a different \
                             approver, which an agent proposal can't carry. Run it there \
                             with --actor and --approver.",
                            fmt_cents(amount),
                            fmt_cents(threshold)
                        ),
                    });
                }
            }
        }

        let summary = builder_summary(name, intent, customer_name.as_deref());
        let memo = format!("{summary} — prompt: \"{prompt}\"");

        let duplicate_warning = self
            .duplicate_entry(conn, debit, credit, amount, &entry_date)?
            .map(|existing| {
                format!(
                    "This looks like a duplicate of entry #{existing} ({} on {entry_date}).",
                    fmt_cents(amount)
                )
            });
        if let Some(warning) = &duplicate_warning {
            if !options.allow_duplicate {
                return Ok(ProposeOutcome::DuplicateWarning {
                    prompt: prompt.to_string(),
                    warning: format!(
                        "{warning} I won't post it twice — say the word and I'll \
                         prepare it anyway."
                    ),
                });
            }
        }

        let credit_note = if name == "record_deposit" {
            Some("a liability, not revenue".to_string())
        } else if name == "complete_production" {
            Some("finished goods, ready to sell".to_string())
        } else {
            None
        };
        let lines = vec![
            ConfirmLine {
                code: debit.to_string(),
                debit: amount,
                credit: 0,
                note: None,
            },
            ConfirmLine {
                code: credit.to_string(),
                debit: 0,
                credit: amount,
                note: credit_note,
            },
        ];
        let confirmation = self.render(
            conn,
            &builder_headline(name, intent, amount, customer_name.as_deref()),
            &[format!("Date: {entry_date}")],
            &lines,
        )?;
        Ok(ProposeOutcome::NeedsConfirmation(Box::new(Proposal {
            prompt: prompt.to_string(),
            intent: name.to_string(),
            summary: summary.clone(),
            confirmation_text: confirmation,
            duplicate_warning,
            gate: GateCall::Dispatch {
                command: name.to_string(),
                args: json!({
                    "amount_cents": amount,
                    "entry_date": entry_date,
                    "memo": memo,
                }),
            },
            draft_invoice_id: None,
        })))
    }

    // -- MRP intents ---------------------------------------------------
    //
    // Planning questions are read-only Answers computed from a live
    // MRP run (MRP_DESIGN.md: plans are never stored). Releasing a
    // planned order is a gated Proposal like every other mutation:
    // the confirmation is rendered from the freshly computed plan,
    // and confirm runs the one release dispatch call.

    /// Resolve the intent's item reference to a catalog SKU: exact
    /// SKU (any case) or an item-name mention.
    fn resolve_sku(&self, ctx: &Ctx, given: Option<&str>) -> Option<String> {
        let given = given?.trim();
        if given.is_empty() {
            return None;
        }
        if let Some(item) = ctx.items.iter().find(|i| i.sku.eq_ignore_ascii_case(given)) {
            return Some(item.sku.clone());
        }
        ctx.items
            .iter()
            .find(|i| i.name.to_lowercase().contains(&given.to_lowercase()))
            .map(|i| i.sku.clone())
    }

    /// The run_mrp / release arguments for the intent's what-if
    /// demand, when it states one. A stated quantity with no item,
    /// or an item + quantity with no date, is a clarification — the
    /// adapter asks rather than guessing a plan.
    fn mrp_run_args(
        &self,
        prompt: &str,
        intent: &Intent,
        ctx: &Ctx,
    ) -> Result<Value, ProposeOutcome> {
        let mut args = json!({ "as_of": self.today });
        let sku = self.resolve_sku(ctx, intent.sku.as_deref());
        match (sku, intent.qty) {
            (None, Some(_)) => Err(ProposeOutcome::Clarification {
                prompt: prompt.to_string(),
                question: "Which item should I plan for? Name a SKU, e.g. BRK-100.".to_string(),
            }),
            (Some(sku), Some(qty)) => {
                let need_date = match intent.need_date.as_deref() {
                    Some(d) if parse_date(d).is_some() => d.to_string(),
                    Some(d) => {
                        return Err(ProposeOutcome::Clarification {
                            prompt: prompt.to_string(),
                            question: format!(
                                "\"{d}\" is not a date I can plan against — use YYYY-MM-DD."
                            ),
                        })
                    }
                    None => match resolve_day(&self.today, intent.day.as_deref()) {
                        Ok(d) if intent.day.is_some() => d,
                        _ => {
                            return Err(ProposeOutcome::Clarification {
                                prompt: prompt.to_string(),
                                question: format!("By when do you need the {sku}?"),
                            })
                        }
                    },
                };
                args["demand_sku"] = json!(sku);
                args["demand_qty"] = json!(qty);
                args["demand_date"] = json!(need_date);
                if let Some(config) = &intent.configuration {
                    args["demand_config"] = config.clone();
                }
                Ok(args)
            }
            _ => Ok(args),
        }
    }

    fn propose_mrp_check(
        &self,
        conn: &mut Connection,
        prompt: &str,
        intent: &Intent,
        ctx: &Ctx,
    ) -> Result<ProposeOutcome, AgentError> {
        let args = match self.mrp_run_args(prompt, intent, ctx) {
            Ok(args) => args,
            Err(outcome) => return Ok(outcome),
        };
        let plan = self.call(conn, "run_mrp", &args)?;
        let mut out = format!(
            "MRP run as of {} — computed live from open orders, stock, BOMs, and \
             configurations; plans are never stored.\n",
            j_str(&plan, "as_of")
        );
        let demand_sku = args.get("demand_sku").and_then(|v| v.as_str());
        if let Some(sku) = demand_sku {
            let netting = plan
                .get("requirements")
                .and_then(|r| r.as_array())
                .and_then(|rows| rows.iter().find(|r| j_str(r, "sku") == sku));
            let planned = plan
                .get("planned_orders")
                .and_then(|r| r.as_array())
                .and_then(|rows| rows.iter().find(|r| j_str(r, "sku") == sku));
            match (netting, planned) {
                (Some(n), Some(o)) => {
                    out.push_str(&format!(
                        "\n{sku}: demand {} by {}. Available {} (on hand {} + on order {} + \
                         WIP {}) covers {}; the plan {} {} more, starting {} (due {}).{}\n",
                        j_i64(n, "gross_required"),
                        j_str(o, "due_date"),
                        j_i64(n, "available"),
                        j_i64(n, "on_hand"),
                        j_i64(n, "on_order"),
                        j_i64(n, "wip"),
                        j_i64(n, "gross_required") - j_i64(n, "net_required"),
                        if j_str(o, "kind") == "production" {
                            "builds"
                        } else {
                            "buys"
                        },
                        j_i64(o, "qty"),
                        j_str(o, "start_date"),
                        j_str(o, "due_date"),
                        if j_i64(o, "days_late") > 0 {
                            format!(
                                " That start is already {} day(s) past — this is a shortage.",
                                j_i64(o, "days_late")
                            )
                        } else {
                            " On time, if it starts then.".to_string()
                        },
                    ));
                    let comps: Vec<String> = o
                        .get("components")
                        .and_then(|c| c.as_array())
                        .map(|rows| {
                            rows.iter()
                                .map(|c| format!("{} ×{}", j_str(c, "sku"), j_i64(c, "qty")))
                                .collect()
                        })
                        .unwrap_or_default();
                    if !comps.is_empty() {
                        out.push_str(&format!("Components needed: {}.\n", comps.join(", ")));
                    }
                }
                (Some(n), None) => {
                    out.push_str(&format!(
                        "\n{sku}: demand {} is fully covered — {} available (on hand {} + \
                         on order {} + WIP {}). Nothing to plan.\n",
                        j_i64(n, "gross_required"),
                        j_i64(n, "available"),
                        j_i64(n, "on_hand"),
                        j_i64(n, "on_order"),
                        j_i64(n, "wip"),
                    ));
                }
                _ => {
                    out.push_str(&format!("\n{sku} has no demand in this plan.\n"));
                }
            }
        }
        let shortages = plan
            .get("shortages")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default();
        if shortages.is_empty() {
            out.push_str("\nShortages: none — every planned order can still start on time.");
        } else {
            out.push_str("\nShortages (planned orders that should already have started):");
            for s in &shortages {
                let reasons: Vec<String> = s
                    .get("reasons")
                    .and_then(|r| r.as_array())
                    .map(|rows| {
                        rows.iter()
                            .map(|r| {
                                format!(
                                    "{} ({} needed, {} covered by stock)",
                                    j_str(r, "origin"),
                                    j_i64(r, "qty"),
                                    j_i64(r, "covered_by_stock")
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push_str(&format!(
                    "\n- {}: {} {} due {} — start {} is {} day(s) late. Driven by: {}",
                    j_str(s, "sku"),
                    j_str(s, "kind"),
                    j_i64(s, "qty"),
                    j_str(s, "due_date"),
                    j_str(s, "start_date"),
                    j_i64(s, "days_late"),
                    reasons.join("; ")
                ));
            }
        }
        if demand_sku.is_none() {
            let planned_count = plan
                .get("planned_orders")
                .and_then(|p| p.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            out.push_str(&format!(
                "\nPlanned orders in this run: {planned_count}. Ask \"Can we build \
                 <qty> <SKU> by <date>?\" for a specific check."
            ));
        }
        Ok(ProposeOutcome::Answer {
            prompt: prompt.to_string(),
            message: out,
        })
    }

    fn propose_mrp_release(
        &self,
        conn: &mut Connection,
        prompt: &str,
        intent: &Intent,
        ctx: &Ctx,
    ) -> Result<ProposeOutcome, AgentError> {
        let Some(key) = intent
            .key
            .as_deref()
            .map(|k| k.trim())
            .filter(|k| !k.is_empty())
        else {
            return Ok(ProposeOutcome::Clarification {
                prompt: prompt.to_string(),
                question: "Which planned order should I release? Planned orders carry \
                           keys like purchase:FST-SET:2026-11-10:60 — ask me \"What are \
                           we short on?\" or run list_planned_orders, then name the key."
                    .to_string(),
            });
        };
        let args = match self.mrp_run_args(prompt, intent, ctx) {
            Ok(args) => args,
            Err(outcome) => return Ok(outcome),
        };
        let listed = self.call(conn, "list_planned_orders", &args)?;
        let order = listed
            .get("planned_orders")
            .and_then(|p| p.as_array())
            .and_then(|rows| rows.iter().find(|o| j_str(o, "key") == key).cloned());
        let Some(order) = order else {
            return Ok(ProposeOutcome::Refusal {
                prompt: prompt.to_string(),
                message: format!(
                    "No planned order with key '{key}' in the current plan — plans are \
                     recomputed live and this one is not in it. Check the key with \
                     list_planned_orders (with the same demand arguments), or ask me \
                     what we're short on."
                ),
            });
        };
        let sku = j_str(&order, "sku").to_string();
        let kind = j_str(&order, "kind").to_string();
        let qty = j_i64(&order, "qty");
        let item_name = ctx
            .items
            .iter()
            .find(|i| i.sku == sku)
            .map(|i| i.name.clone())
            .unwrap_or_default();
        let reasons: Vec<String> = order
            .get("reasons")
            .and_then(|r| r.as_array())
            .map(|rows| {
                rows.iter()
                    .map(|r| {
                        format!(
                            "{} — {} by this plan ({} covered by stock)",
                            j_str(r, "origin"),
                            j_i64(r, "covered_by_plan"),
                            j_i64(r, "covered_by_stock")
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut confirmation = format!(
            "Release planned {kind} order — {sku} ×{qty}\n\
             Item: {sku} — {item_name}\n\
             Due: {} · Start: {}{}\n\
             Netting: gross {} − available {} (on hand {} + on order {} + WIP {}) = {} to {}\n\
             Why:\n{}",
            j_str(&order, "due_date"),
            j_str(&order, "start_date"),
            if j_i64(&order, "days_late") > 0 {
                format!(" ({} day(s) late)", j_i64(&order, "days_late"))
            } else {
                String::new()
            },
            j_i64(&order, "gross_required"),
            j_i64(&order, "on_hand") + j_i64(&order, "on_order") + j_i64(&order, "wip"),
            j_i64(&order, "on_hand"),
            j_i64(&order, "on_order"),
            j_i64(&order, "wip"),
            j_i64(&order, "net_required"),
            if kind == "production" { "build" } else { "buy" },
            reasons
                .iter()
                .map(|r| format!("- {r}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        confirmation.push_str(&format!(
            "\nReleasing creates the {kind} order document and counts these units as {} \
             supply in the next MRP run. No journal entry posts — value moves only when \
             real events happen.",
            if kind == "production" {
                "work-in-progress"
            } else {
                "on-order"
            }
        ));
        let mut gate_args = json!({ "key": key, "as_of": self.today });
        for field in ["demand_sku", "demand_qty", "demand_date", "demand_config"] {
            if let Some(v) = args.get(field) {
                gate_args[field] = v.clone();
            }
        }
        Ok(ProposeOutcome::NeedsConfirmation(Box::new(Proposal {
            prompt: prompt.to_string(),
            intent: "mrp_release".to_string(),
            summary: format!("Release planned {kind} order {key}"),
            confirmation_text: confirmation,
            duplicate_warning: None,
            gate: GateCall::Dispatch {
                command: "release_planned_order".to_string(),
                args: gate_args,
            },
            draft_invoice_id: None,
        })))
    }

    // -- P2P intents ---------------------------------------------------
    //
    // Receiving against a procurement order and paying a supplier
    // (AP) invoice, mirroring the invoice/payment pair on the sales
    // side: resolve against live dispatch reads, render the
    // confirmation from the documents themselves, and gate the one
    // posting call (two, when a match override releases a payment).

    fn suppliers(&self, conn: &mut Connection) -> Result<Vec<(i64, String)>, AgentError> {
        let rows = self.call(conn, "list_suppliers", &json!({}))?;
        Ok(rows
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|s| (j_i64(s, "id"), j_str(s, "name").to_string()))
            .collect())
    }

    /// Supplier resolution, the mirror of [`Agent::require_customer`]:
    /// exact match, a clarification for a near-miss, a refusal with
    /// the list for an unknown name. Never an auto-match.
    fn require_supplier(
        &self,
        suppliers: &[(i64, String)],
        given: Option<&str>,
        prompt: &str,
    ) -> Result<(i64, String), ProposeOutcome> {
        let list = suppliers
            .iter()
            .map(|(_, n)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let Some(given) = given.map(|g| g.trim()).filter(|g| !g.is_empty()) else {
            return Err(ProposeOutcome::Clarification {
                prompt: prompt.to_string(),
                question: format!("Which supplier? On file: {list}."),
            });
        };
        if let Some((id, name)) = suppliers
            .iter()
            .find(|(_, n)| n.eq_ignore_ascii_case(given))
        {
            return Ok((*id, name.clone()));
        }
        let near = suppliers.iter().find(|(_, n)| {
            let (g, s) = (given.to_lowercase(), n.to_lowercase());
            g.contains(&s) || s.contains(&g)
        });
        if let Some((_, suggestion)) = near {
            return Err(ProposeOutcome::Clarification {
                prompt: prompt.to_string(),
                question: format!(
                    "\"{given}\" isn't in the supplier list. Did you mean {suggestion}? \
                     On file: {list}."
                ),
            });
        }
        Err(ProposeOutcome::Refusal {
            prompt: prompt.to_string(),
            message: format!(
                "{given} isn't in the supplier list ({list}). Did you mean one of these, \
                 or add a new supplier?"
            ),
        })
    }

    fn propose_receive_goods(
        &self,
        conn: &mut Connection,
        prompt: &str,
        intent: &Intent,
    ) -> Result<ProposeOutcome, AgentError> {
        // The doors seed the P2P chart extension (2050 GR-IR, 6040
        // PPV) and demo suppliers on open; a library caller may not
        // have. The call is idempotent — ensure it before resolving,
        // exactly as open_door_db does.
        erpcraft_core::seed_procurement_if_empty(conn)
            .map_err(|e| AgentError::Dispatch(DispatchError::internal(e.to_string())))?;
        let orders = self.call(conn, "list_procurement_orders", &json!({}))?;
        let order_rows: Vec<Value> = orders.as_array().cloned().unwrap_or_default();

        // Resolve the purchase order: an explicit id wins; then a
        // named supplier's open orders; then the only open order.
        let order = if let Some(id) = intent.order_id {
            match order_rows.iter().find(|o| j_i64(o, "id") == id) {
                Some(found) => (*found).clone(),
                None => {
                    return Ok(ProposeOutcome::Refusal {
                        prompt: prompt.to_string(),
                        message: format!(
                            "There is no purchase order #{id}. {}",
                            open_po_summary(&order_rows)
                        ),
                    })
                }
            }
        } else {
            let open: Vec<&Value> = order_rows
                .iter()
                .filter(|o| matches!(j_str(o, "status"), "open" | "partially_received"))
                .collect();
            let candidates: Vec<&Value> = match &intent.supplier {
                Some(name) => {
                    let suppliers = self.suppliers(conn)?;
                    let (supplier_id, _) =
                        match self.require_supplier(&suppliers, Some(name), prompt) {
                            Ok(pair) => pair,
                            Err(outcome) => return Ok(outcome),
                        };
                    open.iter()
                        .filter(|o| j_i64(o, "supplier_id") == supplier_id)
                        .cloned()
                        .collect()
                }
                None => open,
            };
            match candidates.len() {
                0 => {
                    return Ok(ProposeOutcome::Refusal {
                        prompt: prompt.to_string(),
                        message: match &intent.supplier {
                            Some(name) => format!(
                                "There is no open purchase order for {name} to receive against."
                            ),
                            None => {
                                "There are no open purchase orders to receive against.".to_string()
                            }
                        },
                    })
                }
                1 => (*candidates[0]).clone(),
                _ => {
                    let list = candidates
                        .iter()
                        .map(|o| {
                            format!(
                                "#{} — {} ({}, {} received)",
                                j_i64(o, "id"),
                                j_str(o, "supplier_name"),
                                fmt_cents(j_i64(o, "total_cents")),
                                fmt_cents(j_i64(o, "received_cents"))
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    return Ok(ProposeOutcome::Clarification {
                        prompt: prompt.to_string(),
                        question: format!(
                            "Several purchase orders are open: {list}. Which one did the \
                             delivery come in on?"
                        ),
                    });
                }
            }
        };
        let order_id = j_i64(&order, "id");
        let supplier_name = j_str(&order, "supplier_name").to_string();
        if !matches!(j_str(&order, "status"), "open" | "partially_received") {
            return Ok(ProposeOutcome::Refusal {
                prompt: prompt.to_string(),
                message: format!(
                    "Purchase order #{order_id} ({supplier_name}) is '{}' — nothing more \
                     can be received against it.",
                    j_str(&order, "status")
                ),
            });
        }

        // Lines still owed, from the order itself.
        let all_lines: Vec<Value> = order
            .get("lines")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let open_lines: Vec<&Value> = all_lines
            .iter()
            .filter(|l| j_i64(l, "qty_received") < j_i64(l, "qty"))
            .collect();
        if open_lines.is_empty() {
            return Ok(ProposeOutcome::Refusal {
                prompt: prompt.to_string(),
                message: format!(
                    "Purchase order #{order_id} ({supplier_name}) is fully received already."
                ),
            });
        }

        // Which lines, how much of each: an item named in the prompt
        // picks its line; a bare quantity needs a single open line;
        // neither means the delivery completes the order.
        let items = self.items(conn)?;
        let wanted_sku = intent.sku.as_deref().and_then(|given| {
            let given = given.trim();
            open_lines
                .iter()
                .find(|l| j_str(l, "sku").eq_ignore_ascii_case(given))
                .map(|l| j_str(l, "sku").to_string())
                .or_else(|| {
                    items
                        .iter()
                        .find(|i| i.name.to_lowercase().contains(&given.to_lowercase()))
                        .map(|i| i.sku.clone())
                })
        });
        let targets: Vec<(&Value, i64)> = match (&wanted_sku, intent.qty) {
            (Some(sku), qty) => {
                let Some(line) = open_lines.iter().find(|l| j_str(l, "sku") == sku.as_str()) else {
                    return Ok(ProposeOutcome::Refusal {
                        prompt: prompt.to_string(),
                        message: format!(
                            "Purchase order #{order_id} has no unreceived line for {sku}."
                        ),
                    });
                };
                let remaining = j_i64(line, "qty") - j_i64(line, "qty_received");
                vec![(*line, qty.unwrap_or(remaining))]
            }
            (None, Some(qty)) => {
                if open_lines.len() > 1 {
                    let list = open_lines
                        .iter()
                        .map(|l| {
                            format!(
                                "{} ({} still owed)",
                                j_str(l, "sku"),
                                j_i64(l, "qty") - j_i64(l, "qty_received")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Ok(ProposeOutcome::Clarification {
                        prompt: prompt.to_string(),
                        question: format!(
                            "Purchase order #{order_id} has several lines still owed: \
                             {list}. Which item did the {qty} arrive for?"
                        ),
                    });
                }
                vec![(open_lines[0], qty)]
            }
            (None, None) => open_lines
                .iter()
                .map(|l| (*l, j_i64(l, "qty") - j_i64(l, "qty_received")))
                .collect(),
        };
        for (line, qty) in &targets {
            let remaining = j_i64(line, "qty") - j_i64(line, "qty_received");
            if *qty <= 0 || *qty > remaining {
                return Ok(ProposeOutcome::Refusal {
                    prompt: prompt.to_string(),
                    message: format!(
                        "Only {remaining} of {} remain unreceived on purchase order \
                         #{order_id} ({} ordered, {} already in) — receiving {qty} is \
                         refused; over-receipt is not allowed.",
                        j_str(line, "sku"),
                        j_i64(line, "qty"),
                        j_i64(line, "qty_received")
                    ),
                });
            }
        }

        // Render from the order + stock views: the inventory account
        // follows each item's make-or-buy exactly as the core posts
        // it (make → Finished Goods, buy → Raw Materials).
        let stock = self.call(conn, "list_stock", &json!({}))?;
        let stock_rows: Vec<Value> = stock.as_array().cloned().unwrap_or_default();
        let mut by_account: Vec<(String, i64, Vec<String>)> = Vec::new();
        let mut gate_lines: Vec<Value> = Vec::new();
        let mut sublines: Vec<String> = Vec::new();
        let mut total: i64 = 0;
        for (line, qty) in &targets {
            let amount = qty * j_i64(line, "unit_price_cents");
            total += amount;
            let make_or_buy = stock_rows
                .iter()
                .find(|s| j_i64(s, "item_id") == j_i64(line, "item_id"))
                .map(|s| j_str(s, "make_or_buy").to_string())
                .unwrap_or_else(|| "buy".to_string());
            let account = if make_or_buy == "make" {
                "1220"
            } else {
                "1200"
            };
            let bit = format!("{} ×{qty}", j_str(line, "sku"));
            match by_account.iter_mut().find(|(code, _, _)| code == account) {
                Some((_, sum, bits)) => {
                    *sum += amount;
                    bits.push(bit);
                }
                None => by_account.push((account.to_string(), amount, vec![bit])),
            }
            sublines.push(format!(
                "{}: {qty} × {} = {} ({} of {} already in)",
                j_str(line, "sku"),
                fmt_cents(j_i64(line, "unit_price_cents")),
                fmt_cents(amount),
                j_i64(line, "qty_received"),
                j_i64(line, "qty")
            ));
            gate_lines.push(json!({ "po_line_id": j_i64(line, "id"), "qty": qty }));
        }
        let mut confirm_lines: Vec<ConfirmLine> = by_account
            .iter()
            .map(|(code, amount, bits)| ConfirmLine {
                code: code.clone(),
                debit: *amount,
                credit: 0,
                note: Some(bits.join(", ")),
            })
            .collect();
        confirm_lines.push(ConfirmLine {
            code: "2050".to_string(),
            debit: 0,
            credit: total,
            note: Some("received, not yet invoiced".to_string()),
        });
        let described = targets
            .iter()
            .map(|(l, q)| format!("{} ×{q}", j_str(l, "sku")))
            .collect::<Vec<_>>()
            .join(", ");
        let confirmation = self.render(
            conn,
            &format!("Receive {described} against purchase order #{order_id} — {supplier_name}"),
            &sublines,
            &confirm_lines,
        )?;
        Ok(ProposeOutcome::NeedsConfirmation(Box::new(Proposal {
            prompt: prompt.to_string(),
            intent: "receive_goods".to_string(),
            summary: format!("Receive goods on purchase order #{order_id} ({supplier_name})"),
            confirmation_text: confirmation,
            duplicate_warning: None,
            gate: GateCall::Dispatch {
                command: "receive_goods".to_string(),
                args: json!({ "order_id": order_id, "lines": gate_lines }),
            },
            draft_invoice_id: None,
        })))
    }

    fn propose_pay_ap_invoice(
        &self,
        conn: &mut Connection,
        prompt: &str,
        intent: &Intent,
        options: &ProposeOptions,
    ) -> Result<ProposeOutcome, AgentError> {
        erpcraft_core::seed_procurement_if_empty(conn)
            .map_err(|e| AgentError::Dispatch(DispatchError::internal(e.to_string())))?;
        let rows = self.call(conn, "list_ap_invoices", &json!({}))?;
        let ap_rows: Vec<Value> = rows.as_array().cloned().unwrap_or_default();

        // Resolve the invoice: explicit id, else the named supplier's
        // one open invoice, else the only open one.
        let invoice = if let Some(id) = intent.ap_invoice_id {
            match ap_rows.iter().find(|inv| j_i64(inv, "id") == id) {
                Some(found) => (*found).clone(),
                None => {
                    return Ok(ProposeOutcome::Refusal {
                        prompt: prompt.to_string(),
                        message: format!("There is no AP invoice #{id}."),
                    })
                }
            }
        } else {
            let open: Vec<&Value> = ap_rows
                .iter()
                .filter(|inv| {
                    matches!(j_str(inv, "status"), "open" | "partially_paid" | "on_hold")
                        && j_i64(inv, "balance_cents") > 0
                })
                .collect();
            let candidates: Vec<&Value> = match &intent.supplier {
                Some(name) => {
                    let suppliers = self.suppliers(conn)?;
                    let (supplier_id, _) =
                        match self.require_supplier(&suppliers, Some(name), prompt) {
                            Ok(pair) => pair,
                            Err(outcome) => return Ok(outcome),
                        };
                    open.iter()
                        .filter(|inv| j_i64(inv, "supplier_id") == supplier_id)
                        .cloned()
                        .collect()
                }
                None => open,
            };
            match candidates.len() {
                0 => {
                    return Ok(ProposeOutcome::Refusal {
                        prompt: prompt.to_string(),
                        message: match &intent.supplier {
                            Some(name) => {
                                format!("{name} has no open AP invoice to pay.")
                            }
                            None => "There are no open AP invoices to pay.".to_string(),
                        },
                    })
                }
                1 => (*candidates[0]).clone(),
                _ => {
                    let list = candidates
                        .iter()
                        .map(|inv| {
                            format!(
                                "#{} ({} due{})",
                                j_i64(inv, "id"),
                                fmt_cents(j_i64(inv, "balance_cents")),
                                if j_str(inv, "status") == "on_hold" {
                                    ", on match hold"
                                } else {
                                    ""
                                }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Ok(ProposeOutcome::Clarification {
                        prompt: prompt.to_string(),
                        question: format!(
                            "{} has {} open AP invoices: {list}. Which one is being paid?",
                            j_str(candidates[0], "supplier_name"),
                            candidates.len()
                        ),
                    });
                }
            }
        };
        let ap_id = j_i64(&invoice, "id");
        let supplier_name = j_str(&invoice, "supplier_name").to_string();
        let balance = j_i64(&invoice, "balance_cents");
        if j_str(&invoice, "status") == "paid" || balance <= 0 {
            return Ok(ProposeOutcome::Refusal {
                prompt: prompt.to_string(),
                message: format!("AP invoice #{ap_id} ({supplier_name}) is already paid in full."),
            });
        }

        let amount = match self.amount_slot(intent, prompt) {
            Ok(amount) => amount,
            Err(outcome) => return Ok(outcome),
        };
        let amount = amount.unwrap_or(balance);
        if amount > balance {
            return Ok(ProposeOutcome::Refusal {
                prompt: prompt.to_string(),
                message: format!(
                    "{} is more than the {} balance on AP invoice #{ap_id} — overpayment \
                     is refused.",
                    fmt_cents(amount),
                    fmt_cents(balance)
                ),
            });
        }

        // Controls: above the approval threshold the posting needs a
        // named actor and a different approver, which an agent
        // proposal cannot carry — refuse up front rather than
        // proposing something confirm would bounce.
        let settings = self.call(conn, "list_control_settings", &json!({}))?;
        let threshold = settings.as_array().and_then(|rows| {
            rows.iter()
                .find(|s| j_str(s, "key") == "payment_approval_threshold_cents")
                .and_then(|s| j_str(s, "value").parse::<i64>().ok())
        });
        if let Some(threshold) = threshold {
            if amount > threshold {
                return Ok(ProposeOutcome::Refusal {
                    prompt: prompt.to_string(),
                    message: format!(
                        "Paying {} is above the {} payment-approval threshold: the \
                         pay_ap_invoice command must name an actor and a different \
                         approver, which an agent proposal can't carry. Run it there \
                         with --actor and --approver.",
                        fmt_cents(amount),
                        fmt_cents(threshold)
                    ),
                });
            }
        }

        // The three-way-match hold: it surfaces before anything is
        // proposed. No reason in the prompt → no proposal at all.
        // Reason but no name → a clarification (the override records
        // who). Reason + name → the proposal carries the override as
        // its first gated step, hold and variances printed on it.
        if j_str(&invoice, "status") == "on_hold" {
            let variances: Vec<String> = invoice
                .get("variances")
                .and_then(|v| v.as_array())
                .map(|rows| {
                    rows.iter()
                        .map(|v| j_str(v, "message").to_string())
                        .collect()
                })
                .unwrap_or_default();
            let hold_line = if variances.is_empty() {
                "a match variance".to_string()
            } else {
                variances.join("; ")
            };
            // Attribution comes from the human, never from the
            // model alone (audit fix, Oct 31): the model *extracts*
            // the override reason and the overrider's name, but both
            // are honored only when they appear in the human's own
            // prompt. A model that invents an authorization the human
            // never wrote produces a refusal, not an override.
            let prompt_lower = prompt.to_lowercase();
            let in_prompt = |s: &str| prompt_lower.contains(&s.to_lowercase());
            let reason = intent
                .override_reason
                .as_deref()
                .map(|r| r.trim())
                .filter(|r| !r.is_empty() && in_prompt(r));
            let overrider = intent
                .overrider
                .as_deref()
                .map(|r| r.trim())
                .filter(|r| !r.is_empty() && in_prompt(r));
            match (reason, overrider) {
                (Some(reason), Some(by)) => {
                    let payment_text = self.render(
                        conn,
                        &format!(
                            "Step 2 — pay {supplier_name} {} against AP invoice #{ap_id}",
                            fmt_cents(amount)
                        ),
                        &[format!("Balance after: {}", fmt_cents(balance - amount))],
                        &[
                            ConfirmLine {
                                code: "2000".to_string(),
                                debit: amount,
                                credit: 0,
                                note: None,
                            },
                            ConfirmLine {
                                code: "1000".to_string(),
                                debit: 0,
                                credit: amount,
                                note: None,
                            },
                        ],
                    )?;
                    let confirmation = format!(
                        "Pay {supplier_name} {} against AP invoice #{ap_id} — \
                         ON THREE-WAY-MATCH HOLD\n\
                         ⚠ Match hold: {hold_line}\n\
                         Step 1 — override the match: reason \"{reason}\" and overrider \
                         {by} are recorded on the invoice; the variance facts stay \
                         visible.\n{payment_text}",
                        fmt_cents(amount)
                    );
                    return Ok(ProposeOutcome::NeedsConfirmation(Box::new(Proposal {
                        prompt: prompt.to_string(),
                        intent: "pay_ap_invoice".to_string(),
                        summary: format!(
                            "Pay AP invoice #{ap_id} ({supplier_name}) — match override by {by}"
                        ),
                        confirmation_text: confirmation,
                        duplicate_warning: None,
                        gate: GateCall::Sequence(vec![
                            (
                                "override_ap_match".to_string(),
                                json!({
                                    "ap_invoice_id": ap_id,
                                    "reason": reason,
                                    "by": by,
                                }),
                            ),
                            (
                                "pay_ap_invoice".to_string(),
                                json!({ "ap_invoice_id": ap_id, "amount_cents": amount }),
                            ),
                        ]),
                        draft_invoice_id: None,
                    })));
                }
                (Some(_), None) => {
                    return Ok(ProposeOutcome::Clarification {
                        prompt: prompt.to_string(),
                        question: format!(
                            "AP invoice #{ap_id} is on three-way-match hold ({hold_line}). \
                             Who is overriding the match? The override is recorded with \
                             that person's name."
                        ),
                    });
                }
                _ => {
                    return Ok(ProposeOutcome::Refusal {
                        prompt: prompt.to_string(),
                        message: format!(
                            "AP invoice #{ap_id} ({supplier_name}) is on three-way-match \
                             hold — {hold_line} — and can't be proposed for payment \
                             while the hold stands. Resolve the variance, run \
                             override_ap_match with a recorded reason, or restate the \
                             payment with the reason and the approver's name in the \
                             prompt (\"pay it anyway — <reason>, approved by <name>\")."
                        ),
                    });
                }
            }
        }

        // Duplicate scan, mirroring the sales-side payment: the same
        // amount already recorded against this invoice warns first.
        let payments = self.call(conn, "list_ap_payments", &json!({ "ap_invoice_id": ap_id }))?;
        let duplicate_warning = payments.as_array().and_then(|rows| {
            rows.iter()
                .find(|p| j_i64(p, "amount_cents") == amount)
                .map(|_| {
                    format!(
                        "A payment of {} against AP invoice #{ap_id} is already recorded.",
                        fmt_cents(amount)
                    )
                })
        });
        if let Some(warning) = &duplicate_warning {
            if !options.allow_duplicate {
                return Ok(ProposeOutcome::DuplicateWarning {
                    prompt: prompt.to_string(),
                    warning: format!(
                        "{warning} I won't post it twice — say the word and I'll \
                         prepare it anyway."
                    ),
                });
            }
        }

        let mut sublines = vec![format!(
            "Invoice total {} — {} already paid; balance after: {}",
            fmt_cents(j_i64(&invoice, "total_cents")),
            fmt_cents(j_i64(&invoice, "paid_cents")),
            fmt_cents(balance - amount)
        )];
        if j_str(&invoice, "match_status") == "overridden" {
            sublines.push(format!(
                "Three-way match was overridden by {}: {}",
                invoice
                    .get("overridden_by")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?"),
                invoice
                    .get("override_reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            ));
        }
        let lines = vec![
            ConfirmLine {
                code: "2000".to_string(),
                debit: amount,
                credit: 0,
                note: Some(format!("{supplier_name}'s balance drops")),
            },
            ConfirmLine {
                code: "1000".to_string(),
                debit: 0,
                credit: amount,
                note: None,
            },
        ];
        let confirmation = self.render(
            conn,
            &format!(
                "Pay {supplier_name} {} against AP invoice #{ap_id}",
                fmt_cents(amount)
            ),
            &sublines,
            &lines,
        )?;
        Ok(ProposeOutcome::NeedsConfirmation(Box::new(Proposal {
            prompt: prompt.to_string(),
            intent: "pay_ap_invoice".to_string(),
            summary: format!("Pay AP invoice #{ap_id} ({supplier_name})"),
            confirmation_text: confirmation,
            duplicate_warning,
            gate: GateCall::Dispatch {
                command: "pay_ap_invoice".to_string(),
                args: json!({ "ap_invoice_id": ap_id, "amount_cents": amount }),
            },
            draft_invoice_id: None,
        })))
    }

    // -- Resolution + scans ------------------------------------------

    fn customers(&self, conn: &mut Connection) -> Result<Vec<(i64, String)>, AgentError> {
        let rows = self.call(conn, "list_customers", &json!({}))?;
        Ok(rows
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|c| (j_i64(c, "id"), j_str(c, "name").to_string()))
            .collect())
    }

    fn items(&self, conn: &mut Connection) -> Result<Vec<ParseItem>, AgentError> {
        let rows = self.call(conn, "list_items", &json!({}))?;
        Ok(rows
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|i| ParseItem {
                id: j_i64(i, "id"),
                sku: j_str(i, "sku").to_string(),
                name: j_str(i, "name").to_string(),
                unit_price_cents: j_i64(i, "unit_price_cents"),
                standard_cost_cents: j_i64(i, "standard_cost_cents"),
            })
            .collect())
    }

    /// Customer resolution (spec rule 4): exact match, or a
    /// clarification for a near-miss, or a refusal with the list for
    /// an unknown name. Never an auto-match.
    fn require_customer(
        &self,
        customers: &[(i64, String)],
        given: Option<&str>,
        prompt: &str,
    ) -> Result<(i64, String), ProposeOutcome> {
        let list = customers
            .iter()
            .map(|(_, n)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let Some(given) = given.map(|g| g.trim()).filter(|g| !g.is_empty()) else {
            return Err(ProposeOutcome::Clarification {
                prompt: prompt.to_string(),
                question: format!("Which customer? On file: {list}."),
            });
        };
        if let Some((id, name)) = customers
            .iter()
            .find(|(_, n)| n.eq_ignore_ascii_case(given))
        {
            return Ok((*id, name.clone()));
        }
        let near = customers.iter().find(|(_, n)| {
            let (g, c) = (given.to_lowercase(), n.to_lowercase());
            g.contains(&c) || c.contains(&g)
        });
        if let Some((_, suggestion)) = near {
            return Err(ProposeOutcome::Clarification {
                prompt: prompt.to_string(),
                question: format!(
                    "\"{given}\" isn't in the customer list. Did you mean {suggestion}? \
                     On file: {list}."
                ),
            });
        }
        Err(ProposeOutcome::Refusal {
            prompt: prompt.to_string(),
            message: format!(
                "{given} isn't in the customer list ({list}). Did you mean one of these, \
                 or add a new customer?"
            ),
        })
    }

    /// The sellable item: the one seeded finished good; if several
    /// ever exist, the BRK-100 — anything else is a question, not a
    /// guess.
    fn require_item(&self, items: &[ParseItem], prompt: &str) -> Result<ParseItem, ProposeOutcome> {
        match items.len() {
            1 => Ok(items[0].clone()),
            _ => items
                .iter()
                .find(|i| i.sku == "BRK-100")
                .cloned()
                .ok_or_else(|| ProposeOutcome::Clarification {
                    prompt: prompt.to_string(),
                    question: "Which item? The prompt doesn't say and I won't guess.".to_string(),
                }),
        }
    }

    /// The amount slot, parsed exactly (rule 1): absent → `None`
    /// (the caller clarifies); present but not exact to the cent →
    /// a refusal outcome.
    fn amount_slot(&self, intent: &Intent, prompt: &str) -> Result<Option<i64>, ProposeOutcome> {
        money_slot(&intent.amount).map_err(|e| ProposeOutcome::Refusal {
            prompt: prompt.to_string(),
            message: e.to_string(),
        })
    }

    /// Fulfilled orders for this customer with no invoice yet —
    /// the candidate pool both matchers below scan.
    fn fulfilled_candidates(
        &self,
        conn: &mut Connection,
        customer_id: i64,
        invoices: &[Value],
    ) -> Result<Vec<Value>, AgentError> {
        let invoiced_orders: Vec<i64> = invoices.iter().map(|i| j_i64(i, "order_id")).collect();
        let orders = self.call(conn, "list_orders", &json!({}))?;
        Ok(orders
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter(|o| {
                        j_i64(o, "customer_id") == customer_id
                            && j_str(o, "status") == "fulfilled"
                            && !invoiced_orders.contains(&j_i64(o, "id"))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    fn order_qty(order: &Value) -> i64 {
        order
            .get("lines")
            .and_then(|v| v.as_array())
            .map(|lines| lines.iter().map(|l| j_i64(l, "qty")).sum())
            .unwrap_or(0)
    }

    /// The fulfilled order that IS this shipment: same customer,
    /// same total, same total quantity.
    fn fulfilled_order_for(
        &self,
        conn: &mut Connection,
        customer_id: i64,
        total: i64,
        qty: i64,
        invoices: &[Value],
    ) -> Result<Option<i64>, AgentError> {
        Ok(self
            .fulfilled_candidates(conn, customer_id, invoices)?
            .iter()
            .find(|o| j_i64(o, "total_cents") == total && Self::order_qty(o) == qty)
            .map(|o| j_i64(o, "id")))
    }

    /// A fulfilled order matching the total but NOT the quantity —
    /// evidence the described shipment is a different one. Returns
    /// (order id, its quantity, whether any line is configured).
    fn fulfilled_qty_mismatch(
        &self,
        conn: &mut Connection,
        customer_id: i64,
        total: i64,
        qty: i64,
        invoices: &[Value],
    ) -> Result<Option<(i64, i64, bool)>, AgentError> {
        Ok(self
            .fulfilled_candidates(conn, customer_id, invoices)?
            .iter()
            .find(|o| j_i64(o, "total_cents") == total && Self::order_qty(o) != qty)
            .map(|o| {
                let configured = o
                    .get("lines")
                    .and_then(|v| v.as_array())
                    .map(|lines| {
                        lines.iter().any(|l| {
                            l.get("configuration")
                                .map(|c| !c.is_null())
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
                (j_i64(o, "id"), Self::order_qty(o), configured)
            }))
    }

    /// The builder duplicate scan (rule 6): a posted entry with the
    /// same two accounts, the same amount and the same date.
    fn duplicate_entry(
        &self,
        conn: &mut Connection,
        debit: &str,
        credit: &str,
        amount: i64,
        entry_date: &str,
    ) -> Result<Option<i64>, AgentError> {
        let entries = self.call(conn, "list_entries", &json!({}))?;
        Ok(entries.as_array().and_then(|rows| {
            rows.iter()
                .find(|e| {
                    j_str(e, "entry_date") == entry_date
                        && j_str(e, "status") == "posted"
                        && e.get("lines").and_then(|v| v.as_array()).map(|lines| {
                            lines.len() == 2
                                && lines.iter().any(|l| {
                                    j_str(l, "account_code") == debit
                                        && j_i64(l, "debit_cents") == amount
                                })
                                && lines.iter().any(|l| {
                                    j_str(l, "account_code") == credit
                                        && j_i64(l, "credit_cents") == amount
                                })
                        }) == Some(true)
                })
                .map(|e| j_i64(e, "id"))
        }))
    }

    /// One dispatch call with the agent's error mapping: an internal
    /// failure is an error; a core refusal becomes
    /// [`AgentError::CoreRefusal`], which `propose` renders as a
    /// refusal outcome in the core's own words.
    fn call(&self, conn: &mut Connection, name: &str, args: &Value) -> Result<Value, AgentError> {
        dispatch(conn, Door::Agent, name, args).map_err(|e| {
            if e.internal {
                AgentError::Dispatch(e)
            } else {
                AgentError::CoreRefusal(e.message)
            }
        })
    }

    // -- Confirmation rendering ---------------------------------------

    /// Render the confirmation from the proposed calls and core data
    /// (account names and types from the ledger) — the model's prose
    /// is never involved. Signs follow each account's normal balance:
    /// a credit to Revenue reads +, a credit to Finished Goods −.
    fn render(
        &self,
        conn: &mut Connection,
        headline: &str,
        sublines: &[String],
        lines: &[ConfirmLine],
    ) -> Result<String, AgentError> {
        let mut accounts: HashMap<String, (String, String)> = HashMap::new();
        for line in lines {
            if !accounts.contains_key(&line.code) {
                let ledger = self.call(conn, "account_ledger", &json!({ "code": line.code }))?;
                accounts.insert(
                    line.code.clone(),
                    (
                        j_str(&ledger, "name").to_string(),
                        j_str(&ledger, "account_type").to_string(),
                    ),
                );
            }
        }
        let mut out = String::new();
        out.push_str(headline);
        out.push('\n');
        for sub in sublines {
            out.push_str(sub);
            out.push('\n');
        }
        let mut total_debits: i64 = 0;
        let mut total_credits: i64 = 0;
        for line in lines {
            total_debits += line.debit;
            total_credits += line.credit;
            let (name, account_type) = &accounts[&line.code];
            let delta = if debit_normal(account_type) {
                line.debit - line.credit
            } else {
                line.credit - line.debit
            };
            let sign = if delta < 0 { "-" } else { "+" };
            out.push_str(&format!("- {name} {sign}{}", fmt_cents(delta.abs())));
            if let Some(note) = &line.note {
                out.push_str(&format!(" ({note})"));
            }
            out.push('\n');
        }
        if total_debits == total_credits {
            out.push_str(&format!(
                "Total debits {} = total credits {} ✓",
                fmt_cents(total_debits),
                fmt_cents(total_credits)
            ));
        } else {
            out.push_str(&format!(
                "Total debits {} ≠ total credits {} — this cannot post",
                fmt_cents(total_debits),
                fmt_cents(total_credits)
            ));
        }
        out.push_str("\n[Confirm] [Edit] [Cancel]");
        Ok(out)
    }
}

struct ConfirmLine {
    code: String,
    debit: i64,
    credit: i64,
    note: Option<String>,
}

fn debit_normal(account_type: &str) -> bool {
    matches!(account_type, "asset" | "expense" | "contra_revenue")
}

/// One line naming the purchase orders still open for receiving
/// (used in refusals so a wrong id comes back with the real list).
fn open_po_summary(order_rows: &[Value]) -> String {
    let open: Vec<String> = order_rows
        .iter()
        .filter(|o| matches!(j_str(o, "status"), "open" | "partially_received"))
        .map(|o| format!("#{} ({})", j_i64(o, "id"), j_str(o, "supplier_name")))
        .collect();
    if open.is_empty() {
        "No purchase orders are open for receiving.".to_string()
    } else {
        format!("Open for receiving: {}.", open.join(", "))
    }
}

fn builder_summary(name: &str, intent: &Intent, customer: Option<&str>) -> String {
    match name {
        "buy_materials" => match &intent.supplier {
            Some(s) => format!("Buy materials on credit (the {s})"),
            None => "Buy materials on credit".to_string(),
        },
        "pay_supplier" => match &intent.supplier {
            Some(s) => format!("Pay supplier (the {s})"),
            None => "Pay supplier".to_string(),
        },
        "issue_materials" => "Issue materials to production".to_string(),
        "record_labor" => "Record direct labor".to_string(),
        "complete_production" => match intent.qty {
            Some(q) => format!("Complete production ({q} brackets)"),
            None => "Complete production".to_string(),
        },
        "record_deposit" => match customer {
            Some(c) => format!("Record customer deposit ({c})"),
            None => "Record customer deposit".to_string(),
        },
        "record_rent" => "Record rent".to_string(),
        "record_interest" => "Record interest".to_string(),
        "record_depreciation" => "Record depreciation".to_string(),
        _ => name.to_string(),
    }
}

fn builder_headline(name: &str, intent: &Intent, amount: i64, customer: Option<&str>) -> String {
    let amount = fmt_cents(amount);
    match name {
        "buy_materials" => format!("Buy {amount} of raw materials on credit"),
        "pay_supplier" => match &intent.supplier {
            Some(s) => format!("Pay {amount} to the {s} against what we owe"),
            None => format!("Pay {amount} to the supplier against what we owe"),
        },
        "issue_materials" => format!("Issue {amount} of raw materials to production"),
        "record_labor" => format!("Pay {amount} wages for production work"),
        "complete_production" => match intent.qty {
            Some(q) => format!("Complete production — {q} brackets finished, cost {amount}"),
            None => format!("Complete production — cost {amount}"),
        },
        "record_deposit" => match customer {
            Some(c) => format!("{c} sent a {amount} deposit for a future order"),
            None => format!("Record a customer deposit of {amount}"),
        },
        "record_rent" => format!("Record rent of {amount}"),
        "record_interest" => format!("Record loan interest of {amount}"),
        "record_depreciation" => format!("Record depreciation of {amount}"),
        _ => format!("{name}: {amount}"),
    }
}

// ---------------------------------------------------------------------------
// Intent JSON, money, dates
// ---------------------------------------------------------------------------

/// The model's structured answer. Every field is optional in the
/// JSON so a partial parse reaches the adapter, which — not the
/// model — decides what is missing (clarify), wrong (refuse) or fine.
#[derive(Debug, Default, Deserialize)]
struct Intent {
    #[serde(default)]
    intent: String,
    #[serde(default)]
    customer: Option<String>,
    #[serde(default)]
    supplier: Option<String>,
    #[serde(default)]
    qty: Option<i64>,
    #[serde(default)]
    amount: Option<Value>,
    #[serde(default)]
    price_each: Option<Value>,
    #[serde(default)]
    day: Option<String>,
    #[serde(default)]
    topic: Option<String>,
    /// MRP intents: the item a planning question is about.
    #[serde(default)]
    sku: Option<String>,
    /// MRP intents: the needed-by date, `YYYY-MM-DD` as written.
    #[serde(default)]
    need_date: Option<String>,
    /// MRP release: the planned-order key, copied exactly.
    #[serde(default)]
    key: Option<String>,
    /// MRP what-if demand: configuration values for a configured
    /// item (characteristic code → value).
    #[serde(default)]
    configuration: Option<Value>,
    /// P2P receive: the purchase order's id, when the prompt names
    /// one ("purchase order 3").
    #[serde(default)]
    order_id: Option<i64>,
    /// P2P payment: the AP invoice's id, when the prompt names one
    /// ("AP invoice 3").
    #[serde(default)]
    ap_invoice_id: Option<i64>,
    /// P2P payment on a match hold: the justification for overriding
    /// the three-way match, copied as stated.
    #[serde(default)]
    override_reason: Option<String>,
    /// P2P payment on a match hold: who approved the override —
    /// recorded on the invoice by `override_ap_match`.
    #[serde(default)]
    overrider: Option<String>,
}

/// Extract the intent object from the model's raw reply: the first
/// `{` to the last `}` (models wrap JSON in fences and prose; the
/// adapter accepts the object and ignores the wrapping).
fn parse_intent_json(raw: &str) -> Option<Intent> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str(&raw[start..=end]).ok()
}

/// A money slot: strings are parsed exactly as written; a bare JSON
/// number is read through its shortest decimal form, so the same
/// exactness rule applies either way. Absent → `None`; present but
/// not exact to the cent → an error the adapter turns into a refusal.
fn money_slot(slot: &Option<Value>) -> Result<Option<i64>, AgentError> {
    match slot {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => parse_money_cents(s).map(Some).map_err(AgentError::Model),
        Some(Value::Number(n)) => parse_money_cents(&n.to_string())
            .map(Some)
            .map_err(AgentError::Model),
        Some(_) => Err(AgentError::Model(
            "the amount was not a money value".to_string(),
        )),
    }
}

/// Rule 1: integer cents, exact parse or rejection. "$5,000.00" →
/// 500000; a third decimal place, a stray letter, or an empty string
/// is a rejection, never a rounding.
pub fn parse_money_cents(raw: &str) -> Result<i64, String> {
    let trimmed = raw.trim();
    let body = trimmed.strip_prefix('$').unwrap_or(trimmed).trim();
    let digits: String = body.chars().filter(|c| *c != ',').collect();
    let (int_part, frac_part) = match digits.split_once('.') {
        Some((a, b)) => (a, b),
        None => (digits.as_str(), ""),
    };
    if int_part.is_empty() || !int_part.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("\"{raw}\" is not an amount I can parse exactly"));
    }
    if frac_part.len() > 2 || !frac_part.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "\"{raw}\" is not exact to the cent — I won't round money"
        ));
    }
    let dollars: i64 = int_part
        .parse()
        .map_err(|_| format!("\"{raw}\" is too large to be an amount"))?;
    let frac: i64 = format!("{frac_part:0<2}").parse().unwrap_or(0);
    Ok(dollars * 100 + frac)
}

/// Money display: two decimals, thousands separators (`$5,000.00`).
pub fn fmt_cents(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    let digits = (abs / 100).to_string();
    let mut grouped = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    format!("{sign}${grouped}.{:02}", abs % 100)
}

/// Resolve a stated day against the demo day (rule 3): "today" or no
/// day → today; a weekday name → its most recent occurrence,
/// including today; a full `YYYY-MM-DD` → itself. The resolved date
/// is shown in the confirmation and passed as the builders'
/// `entry_date`, so the display and the posting can never disagree.
pub fn resolve_day(today: &str, day: Option<&str>) -> Result<String, String> {
    let Some(day) = day.map(|d| d.trim()).filter(|d| !d.is_empty()) else {
        return Ok(today.to_string());
    };
    if day.eq_ignore_ascii_case("today") {
        return Ok(today.to_string());
    }
    if parse_date(day).is_some() {
        return Ok(day.to_string());
    }
    let target = match day.to_lowercase().as_str() {
        "sunday" => 0,
        "monday" => 1,
        "tuesday" => 2,
        "wednesday" => 3,
        "thursday" => 4,
        "friday" => 5,
        "saturday" => 6,
        _ => {
            return Err(format!(
                "Which day did you mean by \"{day}\"? Name a weekday or a date."
            ))
        }
    };
    let (y, m, d) = parse_date(today)
        .ok_or_else(|| format!("the demo day \"{today}\" is not a YYYY-MM-DD date"))?;
    let days = days_from_civil(y, m, d);
    let today_weekday = (days + 4).rem_euclid(7); // 1970-01-01 was a Thursday; 0 = Sunday
    let back = (today_weekday - target).rem_euclid(7);
    Ok(civil_from_days(days - back))
}

/// Today (UTC) as `YYYY-MM-DD` — the CLI door's default demo day.
pub fn today_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    civil_from_days((secs / 86_400) as i64)
}

fn parse_date(text: &str) -> Option<(i64, u32, u32)> {
    let mut parts = text.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, m, d))
}

/// Howard Hinnant's days-from-civil / civil-from-days, the same
/// algorithm the dispatch layer uses for its UTC default date.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = if m > 2 { m as i64 - 3 } else { m as i64 + 9 };
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
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

// JSON field accessors over dispatch results.
fn j_i64(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(|v| v.as_i64()).unwrap_or(0)
}

fn j_str<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_parses_exactly_or_not_at_all() {
        assert_eq!(parse_money_cents("$5,000.00").unwrap(), 500_000);
        assert_eq!(parse_money_cents("$3,125").unwrap(), 312_500);
        assert_eq!(parse_money_cents("1200").unwrap(), 120_000);
        assert_eq!(parse_money_cents("$1.5").unwrap(), 150);
        assert!(
            parse_money_cents("$1.234").is_err(),
            "a third decimal is a rejection"
        );
        assert!(parse_money_cents("abc").is_err());
        assert!(parse_money_cents("").is_err());
        assert!(parse_money_cents("$").is_err());
    }

    #[test]
    fn money_formats_with_separators_and_two_decimals() {
        assert_eq!(fmt_cents(500_000), "$5,000.00");
        assert_eq!(fmt_cents(772_000), "$7,720.00");
        assert_eq!(fmt_cents(150), "$1.50");
        assert_eq!(fmt_cents(-272_000), "-$2,720.00");
        assert_eq!(fmt_cents(0), "$0.00");
    }

    #[test]
    fn relative_days_resolve_against_the_demo_day() {
        // Friday 2026-10-30: Tuesday was the 27th.
        assert_eq!(
            resolve_day("2026-10-30", Some("Tuesday")).unwrap(),
            "2026-10-27"
        );
        assert_eq!(
            resolve_day("2026-10-30", Some("Friday")).unwrap(),
            "2026-10-30"
        );
        assert_eq!(
            resolve_day("2026-10-30", Some("today")).unwrap(),
            "2026-10-30"
        );
        assert_eq!(resolve_day("2026-10-30", None).unwrap(), "2026-10-30");
        assert_eq!(
            resolve_day("2026-10-30", Some("2026-10-01")).unwrap(),
            "2026-10-01"
        );
        assert!(resolve_day("2026-10-30", Some("next week")).is_err());
    }

    #[test]
    fn civil_math_round_trips() {
        for (y, m, d) in [(1970, 1, 1), (2024, 1, 1), (2026, 10, 30), (2000, 2, 29)] {
            let days = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(days), format!("{y:04}-{m:02}-{d:02}"));
        }
    }
}
