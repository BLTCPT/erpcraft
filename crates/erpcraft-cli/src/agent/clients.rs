//! Model clients for the Day-5 agent (docs/DAY5_PREP.md): the trait, the
//! deterministic stub, the recorded client, the fallback chain, and
//! the live OpenAI-compatible client.
//!
//! The model only ever returns a structured intent + arguments. It
//! never writes journal lines, never computes a balance, and its
//! prose is never shown as a confirmation — the adapter renders the
//! confirmation from the proposed dispatch calls and core data.
//!
//! Routes:
//! * **Stub** — a deterministic parser for the contract's prompt
//!   shapes. Runs the acceptance suite offline and doubles as the
//!   demo fallback below the recorded client (spec test 12).
//! * **Recorded** — replays a real captured exchange (the T1 demo
//!   exchange lives in `fixtures/recorded_t1.json`).
//! * **Live** — Baseten Muse Spark Open (the $100 grant, verified;
//!   the exact model name is `meta-models/muse-spark-open-1026`) or
//!   the Meta Model API (the sprint plan's primary route, used once
//!   its $150 credit has landed). Keys come from the operator's
//!   environment at runtime; no key is ever stored in this repo.
//! * **Fallback** — primary, else recorded: an unavailable model
//!   degrades to the recorded exchange or a plain refusal pointing at
//!   the CLI door — never a crash, never a silent post.

use super::{AgentError, ParseRequest};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// A model client turns one parse request into the model's raw reply
/// (expected: one JSON object, the intent shape in
/// [`render_parse_messages`]).
pub trait ModelClient {
    fn parse_intent(&self, request: &ParseRequest) -> Result<String, AgentError>;
}

impl ModelClient for Box<dyn ModelClient> {
    fn parse_intent(&self, request: &ParseRequest) -> Result<String, AgentError> {
        (**self).parse_intent(request)
    }
}

/// The messages every live route sends: a fixed system contract plus
/// the ledger context the adapter gathered (customers, items, open
/// invoices) and the verbatim prompt. The stub and the tests build
/// the same request, so all routes parse against identical context.
pub fn render_parse_messages(request: &ParseRequest) -> (String, String) {
    let system = "\
You are the parsing layer of ERPCraft's natural-language agent. You turn one \
operator prompt into ONE JSON object and nothing else — no prose, no journal \
lines. The adapter validates everything and renders the confirmation from the \
ledger, never from your text.

Return exactly this shape, omitting fields that do not apply — emit the \
JSON immediately, without deliberating over the omitted fields:
{\"intent\": \"...\", \"customer\": \"...\", \"supplier\": \"...\", \"qty\": 0, \
\"amount\": \"...\", \"price_each\": \"...\", \"day\": \"...\", \"topic\": \"...\", \
\"sku\": \"...\", \"need_date\": \"...\", \"key\": \"...\", \"configuration\": {...}, \
\"order_id\": 0, \"ap_invoice_id\": 0, \"override_reason\": \"...\", \
\"overrider\": \"...\"}

Intents: invoice_shipment, record_payment, buy_materials, pay_supplier, \
issue_materials, record_labor, complete_production, record_deposit, \
record_return, record_rent, record_interest, record_depreciation, \
mrp_check, mrp_release, receive_goods, pay_ap_invoice. Anything else \
(payroll, tax, multi-currency) is \
{\"intent\": \"out_of_scope\", \"topic\": \"...\"}. If you cannot tell, \
{\"intent\": \"unclear\"}.

Rules:
- Copy customer names and money exactly as written in the prompt (\"$3,125\" \
stays \"$3,125\"). Never convert money to a number, never round it.
- qty is the unit count as an integer, when the prompt states one.
- price_each only when the prompt states a per-unit price (\"at $130 each\").
- day is the weekday name as written (\"Tuesday\") or \"today\".
- Planning questions are mrp_check: \"What are we short on?\" is a plain \
{\"intent\": \"mrp_check\"}; \"Can we build 200 BRK-100 by Nov 15?\" adds \
\"sku\" (copied exactly), \"qty\", and \"need_date\" as YYYY-MM-DD (resolve \
month names against today; a named date already past this year means next \
year). A configured what-if (\"in steel with galvanized finish\") also \
carries \"configuration\": {characteristic code: value}.
- Releasing a planned order is mrp_release: the prompt names the planned \
order's key verbatim (it looks like purchase:FST-SET:2026-11-10:60) — copy \
it into \"key\" exactly, and include sku/qty/need_date when the prompt \
restates the demand the plan was computed from.
- Receiving a delivery against a purchase order is receive_goods (creating \
a purchase order is out_of_scope): include \"order_id\" when the prompt \
names the order number, \"supplier\" (copied exactly) when it names the \
supplier, \"sku\" when it names an item, and \"qty\" when it states how \
many arrived. Omit what the prompt does not state — never invent an id.
- Paying one specific supplier invoice is pay_ap_invoice — not \
pay_supplier, which pays down supplier balances in general. Include \
\"ap_invoice_id\" when the prompt names one (\"AP invoice 3\"), \
\"supplier\" (copied exactly) when it names the supplier, and \"amount\" \
only when the prompt states one (no amount means the full balance). If \
the prompt insists on paying despite a three-way-match hold (\"pay it \
anyway\"), also include \"override_reason\" — the justification exactly \
as stated — and \"overrider\" — the person the prompt names as approving \
it. Without both, a held invoice cannot be proposed for payment. Parse \
a payment the same way whether or not the context shows the invoice \
on a match hold — the adapter surfaces holds itself; a hold is never \
a reason to answer \"unclear\"."
        .to_string();

    let customers = if request.customers.is_empty() {
        "none".to_string()
    } else {
        request.customers.join(", ")
    };
    let items = if request.items.is_empty() {
        "none".to_string()
    } else {
        request
            .items
            .iter()
            .map(|i| {
                format!(
                    "{} {} — price {}, standard cost {}",
                    i.sku,
                    i.name,
                    super::fmt_cents(i.unit_price_cents),
                    super::fmt_cents(i.standard_cost_cents)
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    let invoices = if request.open_invoices.is_empty() {
        "none".to_string()
    } else {
        request
            .open_invoices
            .iter()
            .map(|i| {
                format!(
                    "#{} {} — total {}, balance {}",
                    i.id,
                    i.customer_name,
                    super::fmt_cents(i.total_cents),
                    super::fmt_cents(i.balance_cents)
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    let suppliers = if request.suppliers.is_empty() {
        "none".to_string()
    } else {
        request.suppliers.join(", ")
    };
    let purchase_orders = if request.open_purchase_orders.is_empty() {
        "none".to_string()
    } else {
        request
            .open_purchase_orders
            .iter()
            .map(|po| {
                format!(
                    "#{} {} — total {}, {} received",
                    po.id,
                    po.supplier_name,
                    super::fmt_cents(po.total_cents),
                    super::fmt_cents(po.received_cents)
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    let ap_invoices = if request.open_ap_invoices.is_empty() {
        "none".to_string()
    } else {
        request
            .open_ap_invoices
            .iter()
            .map(|inv| {
                format!(
                    "#{} {} — total {}, balance {}{}",
                    inv.id,
                    inv.supplier_name,
                    super::fmt_cents(inv.total_cents),
                    super::fmt_cents(inv.balance_cents),
                    if inv.status == "on_hold" {
                        " (ON THREE-WAY-MATCH HOLD)"
                    } else {
                        ""
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    let user = format!(
        "Today is {}.\nCustomers on file: {customers}\nItems: {items}\nOpen invoices: {invoices}\n\
         Suppliers on file: {suppliers}\nOpen purchase orders: {purchase_orders}\n\
         Open AP invoices: {ap_invoices}\nPrompt: {}",
        request.today, request.prompt
    );
    (system, user)
}

// ---------------------------------------------------------------------------
// Stub client — deterministic, offline.
// ---------------------------------------------------------------------------

/// A deterministic stand-in for the model: it recognizes the
/// contract spec's prompt shapes with plain text rules and returns
/// the intent JSON a well-behaved model would. It exists so the
/// acceptance suite is reproducible with no network, and so the
/// demo always has a working parse path (spec test 12).
#[derive(Debug, Default, Clone, Copy)]
pub struct StubClient;

impl ModelClient for StubClient {
    fn parse_intent(&self, request: &ParseRequest) -> Result<String, AgentError> {
        Ok(stub_parse(&request.prompt, &request.today, &request.suppliers).to_string())
    }
}

/// The stub's parse of one prompt into the intent JSON shape.
pub fn stub_parse(prompt: &str, today: &str, suppliers: &[String]) -> Value {
    let lower = prompt.to_lowercase();
    let amount = first_money(prompt);
    let qty = stated_qty(prompt);
    let day = stated_day(prompt);

    // MRP intents (MRP_DESIGN.md): a release names its key verbatim;
    // planning questions are read-only checks.
    if let Some(key) = stated_key(prompt) {
        let (sku, qty, need_date) = mrp_demand_fields(prompt, today);
        return json!({
            "intent": "mrp_release",
            "key": key,
            "sku": sku,
            "qty": qty,
            "need_date": need_date,
        });
    }
    if lower.contains("short") || lower.contains("can we build") || lower.contains("can we make") {
        let (sku, qty, need_date) = mrp_demand_fields(prompt, today);
        return json!({
            "intent": "mrp_check",
            "sku": sku,
            "qty": qty,
            "need_date": need_date,
        });
    }
    if lower.contains("purchase order") && !lower.contains("receive") {
        return json!({ "intent": "out_of_scope", "topic": "purchase orders" });
    }
    // P2P intents: receiving a delivery, and paying one named
    // supplier invoice (distinct from the pay_supplier builder,
    // which pays down a balance in general and names no invoice).
    if lower.contains("receive") {
        let (sku, qty, _) = mrp_demand_fields(prompt, today);
        return json!({
            "intent": "receive_goods",
            "order_id": stated_id_after(prompt, &["purchase order", "order", "po"]),
            "supplier": stated_supplier_name(prompt, suppliers),
            "sku": sku,
            "qty": qty,
        });
    }
    if lower.contains("pay")
        && (lower.contains("ap invoice")
            || (lower.contains("invoice") && stated_supplier_name(prompt, suppliers).is_string()))
    {
        let (override_reason, overrider) = if lower.contains("anyway") || lower.contains("override")
        {
            stated_override(prompt)
        } else {
            (Value::Null, Value::Null)
        };
        return json!({
            "intent": "pay_ap_invoice",
            "ap_invoice_id": stated_id_after(prompt, &["ap invoice", "invoice"]),
            "supplier": stated_supplier_name(prompt, suppliers),
            "amount": amount,
            "override_reason": override_reason,
            "overrider": overrider,
        });
    }
    if lower.contains("returned") {
        return json!({
            "intent": "record_return",
            "customer": leading_party(prompt, "returned"),
            "qty": qty,
        });
    }
    if lower.contains("deposit") {
        return json!({
            "intent": "record_deposit",
            "customer": leading_party(prompt, "sent"),
            "amount": amount,
        });
    }
    if lower.contains("paid") && lower.contains("invoice") {
        return json!({
            "intent": "record_payment",
            "customer": leading_party(prompt, "paid"),
            "amount": amount,
        });
    }
    if lower.contains("buy") && lower.contains("raw materials") {
        return json!({
            "intent": "buy_materials",
            "amount": amount,
            "supplier": stated_supplier(prompt),
        });
    }
    if lower.contains("pay") && (lower.contains("supplier") || lower.contains("we owe")) {
        return json!({
            "intent": "pay_supplier",
            "amount": amount,
            "supplier": stated_supplier(prompt),
        });
    }
    if has_word(&lower, "issue") && lower.contains("raw materials") {
        return json!({ "intent": "issue_materials", "amount": amount });
    }
    if lower.contains("wages") {
        return json!({ "intent": "record_labor", "amount": amount });
    }
    if lower.contains("finished") {
        return json!({
            "intent": "complete_production",
            "qty": qty,
            "amount": amount,
        });
    }
    if lower.contains("rent") {
        return json!({ "intent": "record_rent", "amount": amount });
    }
    if lower.contains("interest") {
        return json!({ "intent": "record_interest", "amount": amount });
    }
    if lower.contains("depreciation") {
        return json!({ "intent": "record_depreciation", "amount": amount });
    }
    if lower.contains("invoice") {
        return json!({
            "intent": "invoice_shipment",
            "customer": invoice_party(prompt),
            "qty": qty,
            "day": day,
            "price_each": price_each(prompt),
        });
    }
    json!({ "intent": "unclear" })
}

/// A planned-order key named in the prompt, verbatim
/// (`production:BRK-100:2026-11-15:150`).
fn stated_key(prompt: &str) -> Option<String> {
    prompt
        .split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| !c.is_alphanumeric() && c != ':' && c != '-')
                .to_string()
        })
        .find(|t| {
            (t.starts_with("production:") || t.starts_with("purchase:"))
                && t.matches(':').count() == 3
        })
}

/// SKU-shaped token: starts with a letter, contains a dash and a
/// digit, and is not a date (`BRK-100`, `FST-SET` — but the fastener
/// set has no digit; two out of three signals suffice when the token
/// is all-caps with a dash).
fn is_sku_token(t: &str) -> bool {
    if t.contains(':') {
        return false; // a planned-order key, not a SKU
    }
    if t.len() < 4 || !t.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    if t.chars().take(4).all(|c| c.is_ascii_digit()) {
        return false; // a date or a year
    }
    t.contains('-')
        && (t.chars().any(|c| c.is_ascii_digit()) || t.chars().all(|c| !c.is_ascii_lowercase()))
}

fn is_iso_date(t: &str) -> bool {
    let parts: Vec<&str> = t.split('-').collect();
    parts.len() == 3
        && parts[0].len() == 4
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// "by Nov 15" → the next such date on or after today, YYYY-MM-DD.
fn month_day_date(prompt: &str, today: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    let words: Vec<String> = prompt
        .to_lowercase()
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .collect();
    let today_parts: Vec<i64> = today.split('-').filter_map(|p| p.parse().ok()).collect();
    if today_parts.len() != 3 {
        return None;
    }
    let (ty, tm, td) = (today_parts[0], today_parts[1], today_parts[2]);
    for (i, w) in words.iter().enumerate() {
        if w.len() < 3 {
            continue;
        }
        let Some(month) = MONTHS.iter().position(|m| m.starts_with(w.as_str())) else {
            continue;
        };
        let month = month as i64 + 1;
        if let Some(day) = words.get(i + 1).and_then(|d| d.parse::<i64>().ok()) {
            let year = if (month, day) < (tm, td) { ty + 1 } else { ty };
            return Some(format!("{year:04}-{month:02}-{day:02}"));
        }
    }
    None
}

/// The (sku, qty, need_date) a planning prompt states, as JSON
/// values (null when unstated — the adapter clarifies, never
/// invents). The quantity is the integer next to the SKU, else the
/// first bare integer that is not part of a date.
fn mrp_demand_fields(prompt: &str, today: &str) -> (Value, Value, Value) {
    let tokens: Vec<String> = prompt
        .split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != ':')
                .to_string()
        })
        .collect();
    let sku_idx = tokens.iter().position(|t| is_sku_token(t));
    let sku = sku_idx.map(|i| tokens[i].clone());
    let mut qty: Option<i64> = None;
    if let Some(i) = sku_idx {
        for cand in [i.wrapping_sub(1), i + 1] {
            if let Some(tok) = tokens.get(cand) {
                if let Ok(n) = tok.replace(',', "").parse::<i64>() {
                    qty = Some(n);
                    break;
                }
            }
        }
    }
    if qty.is_none() {
        qty = tokens
            .iter()
            .filter(|t| !is_iso_date(t))
            .find_map(|t| t.replace(',', "").parse::<i64>().ok());
    }
    let need_date = tokens
        .iter()
        .find(|t| is_iso_date(t))
        .cloned()
        .or_else(|| month_day_date(prompt, today));
    (
        sku.map_or(Value::Null, |s| json!(s)),
        qty.map_or(Value::Null, |q| json!(q)),
        need_date.map_or(Value::Null, |d| json!(d)),
    )
}

/// The first `$…` amount in the prompt, verbatim (the adapter parses
/// it exactly — parsing is never the model's job).
fn first_money(prompt: &str) -> Value {
    match money_tokens(prompt).into_iter().next() {
        Some((_, text)) => Value::String(text),
        None => Value::Null,
    }
}

/// All `$…` tokens with their byte offsets, exactly as written:
/// `$`, digits, commas, and a decimal part when a digit follows the
/// point (a sentence-ending period is not part of the amount).
fn money_tokens(prompt: &str) -> Vec<(usize, String)> {
    let bytes = prompt.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let start = i;
            i += 1;
            let digits_start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b',') {
                i += 1;
            }
            if i > digits_start {
                if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
                    i += 1;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                out.push((start, prompt[start..i].to_string()));
                continue;
            }
        }
        i += 1;
    }
    out
}

/// The per-unit price when the prompt states one ("at $130 each"):
/// the money token immediately before the word "each".
fn price_each(prompt: &str) -> Value {
    let lower = prompt.to_lowercase();
    let Some(each_at) = lower.find(" each") else {
        return Value::Null;
    };
    match money_tokens(prompt)
        .into_iter()
        .rev()
        .find(|(start, _)| *start < each_at)
    {
        Some((_, text)) => Value::String(text),
        None => Value::Null,
    }
}

/// A stated unit count: a number just before "units" or "brackets"
/// ("40 units", "50 brackets", "2 defective brackets"). Absent →
/// null, and the adapter clarifies (spec rule 5) rather than
/// guessing.
fn stated_qty(prompt: &str) -> Value {
    let words: Vec<&str> = prompt.split_whitespace().collect();
    for (i, word) in words.iter().enumerate() {
        let unit = word
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        if unit == "units" || unit == "brackets" || unit == "unit" || unit == "bracket" {
            // The count sits up to two words back ("2 defective brackets").
            for back in 1..=2 {
                if i >= back {
                    let number =
                        words[i - back].trim_matches(|c: char| !c.is_alphanumeric() && c != ',');
                    if let Ok(n) = number.replace(',', "").parse::<i64>() {
                        return json!(n);
                    }
                }
            }
        }
    }
    Value::Null
}

const WEEKDAYS: [&str; 7] = [
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];

/// A stated day: a weekday name as written, or "today".
fn stated_day(prompt: &str) -> Value {
    let lower = prompt.to_lowercase();
    for word in lower.split_whitespace() {
        let clean = word.trim_matches(|c: char| !c.is_alphanumeric());
        if WEEKDAYS.contains(&clean) || clean == "today" {
            let mut chars = clean.chars();
            let capitalized = match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            };
            return Value::String(capitalized);
        }
    }
    Value::Null
}

/// The party named before a verb: "Delta paid …" → "Delta",
/// "Epsilon sent …" → "Epsilon". Leading capitalized words only.
fn leading_party(prompt: &str, verb: &str) -> Value {
    let words: Vec<&str> = prompt.split_whitespace().collect();
    let Some(at) = words.iter().position(|w| {
        w.trim_matches(|c: char| !c.is_alphanumeric())
            .eq_ignore_ascii_case(verb)
    }) else {
        return Value::Null;
    };
    let party: Vec<&str> = words[..at]
        .iter()
        .take_while(|w| w.chars().next().map(|c| c.is_uppercase()).unwrap_or(false))
        .cloned()
        .collect();
    if party.is_empty() {
        Value::Null
    } else {
        Value::String(
            party
                .join(" ")
                .trim_matches(|c: char| !c.is_alphanumeric())
                .to_string(),
        )
    }
}

/// The party named after "Invoice": words up to a stopword, a number
/// or a money token — "Invoice Acme Manufacturing for 10 brackets"
/// keeps both words, so the adapter can clarify the near-miss instead
/// of the stub silently matching "Acme".
fn invoice_party(prompt: &str) -> Value {
    let words: Vec<&str> = prompt.split_whitespace().collect();
    let Some(at) = words.iter().position(|w| {
        w.trim_matches(|c: char| !c.is_alphanumeric())
            .eq_ignore_ascii_case("invoice")
    }) else {
        return Value::Null;
    };
    const STOPWORDS: [&str; 8] = ["for", "paid", "the", "units", "brackets", "at", "on", "a"];
    let mut party: Vec<String> = Vec::new();
    for word in &words[at + 1..] {
        let clean = word.trim_matches(|c: char| !c.is_alphanumeric());
        if clean.is_empty()
            || STOPWORDS.contains(&clean.to_lowercase().as_str())
            || clean
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
            || word.starts_with('$')
        {
            break;
        }
        party.push(clean.to_string());
        if party.len() == 3 {
            break;
        }
    }
    if party.is_empty() {
        Value::Null
    } else {
        Value::String(party.join(" "))
    }
}

/// The supplier as named, when the prompt names the steel supplier —
/// proposal text only; no posting ever carries it.
fn stated_supplier(prompt: &str) -> Value {
    if prompt.to_lowercase().contains("steel supplier") {
        Value::String("steel supplier".to_string())
    } else {
        Value::Null
    }
}

/// A supplier from the parse context named in the prompt: the full
/// name, or its distinctive first word ("SteelWorks" for SteelWorks
/// Supply). The adapter re-resolves the name against the live list,
/// so a near-miss still clarifies there rather than matching here.
fn stated_supplier_name(prompt: &str, suppliers: &[String]) -> Value {
    let lower = prompt.to_lowercase();
    for name in suppliers {
        if lower.contains(&name.to_lowercase()) {
            return Value::String(name.clone());
        }
        let first = name.split_whitespace().next().unwrap_or("");
        if first.len() >= 4 && lower.contains(&first.to_lowercase()) {
            return Value::String(name.clone());
        }
    }
    Value::Null
}

/// The id number stated right after one of the marker phrases
/// ("purchase order 3", "AP invoice 1", "#2"): the first integer
/// token following the marker word(s).
fn stated_id_after(prompt: &str, markers: &[&str]) -> Value {
    let words: Vec<String> = prompt
        .split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric() && c != '#')
                .to_lowercase()
        })
        .collect();
    for marker in markers {
        let marker_words: Vec<&str> = marker.split_whitespace().collect();
        for start in 0..words.len() {
            let hits = marker_words
                .iter()
                .enumerate()
                .all(|(off, mw)| words.get(start + off).map(|w| w.as_str()) == Some(mw));
            if hits {
                if let Some(next) = words.get(start + marker_words.len()) {
                    let digits = next.trim_start_matches('#');
                    if let Ok(n) = digits.parse::<i64>() {
                        return json!(n);
                    }
                }
            }
        }
    }
    Value::Null
}

/// The override pair from an insistent payment prompt ("pay it
/// anyway — the price increase was approved by Dana"): the
/// justification as stated (the clause after "because" or "—",
/// minus the approval tail) and the approver (the capitalized name
/// after the last "by").
fn stated_override(prompt: &str) -> (Value, Value) {
    let lower = prompt.to_lowercase();
    let mut overrider = Value::Null;
    let words: Vec<&str> = prompt.split_whitespace().collect();
    for (i, word) in words.iter().enumerate() {
        let clean = word.trim_matches(|c: char| !c.is_alphanumeric());
        if clean.eq_ignore_ascii_case("by") {
            if let Some(name) = words.get(i + 1) {
                let name = name.trim_matches(|c: char| !c.is_alphanumeric());
                if name.chars().next().is_some_and(|c| c.is_uppercase()) {
                    overrider = Value::String(name.to_string());
                }
            }
        }
    }
    let mut reason: Option<String> = None;
    if let Some(at) = lower.find("because") {
        reason = Some(prompt[at + "because".len()..].trim().to_string());
    } else if let Some(at) = prompt.find('—') {
        reason = Some(prompt[at + '—'.len_utf8()..].trim().to_string());
    }
    let reason = reason.map(|r| {
        let r = r.trim_end_matches('.').trim();
        // Drop the approval tail: "… was approved by Dana" keeps the
        // justification, not the attribution (that rides separately).
        for tail in [" was approved by ", ", approved by ", " approved by "] {
            if let Some(at) = r.to_lowercase().find(tail) {
                return r[..at].trim().trim_end_matches(',').trim().to_string();
            }
        }
        r.to_string()
    });
    (reason.map_or(Value::Null, Value::String), overrider)
}

fn has_word(lower: &str, word: &str) -> bool {
    lower
        .split_whitespace()
        .any(|w| w.trim_matches(|c: char| !c.is_alphanumeric()) == word)
}

// ---------------------------------------------------------------------------
// Recorded client — replays a captured exchange.
// ---------------------------------------------------------------------------

/// A captured model exchange: prompt → the model's raw reply.
/// Recorded from a live call (see `fixtures/recorded_t1.json`) so the
/// demo can replay the T1 flow with no network, under the same
/// confirmation gate — a replay proposes; it never posts on its own.
#[derive(Debug, Clone, Default)]
pub struct RecordedClient {
    exchanges: BTreeMap<String, String>,
}

impl RecordedClient {
    pub fn new(exchanges: BTreeMap<String, String>) -> Self {
        RecordedClient { exchanges }
    }

    /// Load the fixture shape: `{"exchanges": [{"prompt": …,
    /// "response": …}]}` — response may be a string or a JSON value.
    pub fn from_json(text: &str) -> Result<Self, AgentError> {
        let parsed: Value = serde_json::from_str(text).map_err(|e| {
            AgentError::Model(format!(
                "the recorded-response fixture is not valid JSON: {e}"
            ))
        })?;
        let mut exchanges = BTreeMap::new();
        let entries = parsed
            .get("exchanges")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                AgentError::Model(
                    "the recorded-response fixture has no 'exchanges' array".to_string(),
                )
            })?;
        for entry in entries {
            let prompt = entry
                .get("prompt")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    AgentError::Model("a recorded exchange is missing its 'prompt'".to_string())
                })?;
            let response = match entry.get("response") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => {
                    return Err(AgentError::Model(
                        "a recorded exchange is missing its 'response'".to_string(),
                    ))
                }
            };
            exchanges.insert(prompt.to_string(), response);
        }
        Ok(RecordedClient { exchanges })
    }
}

impl ModelClient for RecordedClient {
    fn parse_intent(&self, request: &ParseRequest) -> Result<String, AgentError> {
        self.exchanges.get(&request.prompt).cloned().ok_or_else(|| {
            AgentError::ModelUnavailable(format!(
                "no recorded response covers the prompt \"{}\"",
                request.prompt
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// Fallback chain + the always-down client (spec test 12).
// ---------------------------------------------------------------------------

/// Primary model, else the fallback (recorded responses). Either
/// way the confirmation gate stands — a degraded parse path changes
/// where the intent came from, never whether a human confirms.
#[derive(Debug, Clone)]
pub struct FallbackClient<P: ModelClient, F: ModelClient> {
    primary: P,
    fallback: F,
}

impl<P: ModelClient, F: ModelClient> FallbackClient<P, F> {
    pub fn new(primary: P, fallback: F) -> Self {
        FallbackClient { primary, fallback }
    }
}

impl<P: ModelClient, F: ModelClient> ModelClient for FallbackClient<P, F> {
    fn parse_intent(&self, request: &ParseRequest) -> Result<String, AgentError> {
        match self.primary.parse_intent(request) {
            Ok(reply) => Ok(reply),
            Err(primary_error) => match self.fallback.parse_intent(request) {
                Ok(reply) => Ok(reply),
                Err(fallback_error) => Err(AgentError::ModelUnavailable(format!(
                    "the model is unavailable ({primary_error}) and {fallback_error} — \
                     the same commands are available through the erpcraft CLI"
                ))),
            },
        }
    }
}

/// A client that is always down — the acceptance suite's simulated
/// outage for test 12, and a honest placeholder when no route is
/// configured.
#[derive(Debug, Clone)]
pub struct UnavailableClient {
    pub reason: String,
}

impl ModelClient for UnavailableClient {
    fn parse_intent(&self, _request: &ParseRequest) -> Result<String, AgentError> {
        Err(AgentError::ModelUnavailable(self.reason.clone()))
    }
}

// ---------------------------------------------------------------------------
// Live client — OpenAI-compatible chat completions.
// ---------------------------------------------------------------------------

/// The live route: an OpenAI-compatible `/chat/completions` call.
/// The key is supplied by the operator at runtime (environment) —
/// never stored in the repo, never logged.
#[derive(Clone)]
pub struct OpenAiChatClient {
    base_url: String,
    model: String,
    api_key: String,
    max_tokens: u32,
}

impl OpenAiChatClient {
    pub fn new(base_url: &str, model: &str, api_key: String) -> Self {
        OpenAiChatClient {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            api_key,
            // Generous: the model spends tokens on reasoning before
            // it answers (the Baseten grant note, verified Oct 26).
            // The Oct 31 live eval (LIVE_EVAL.md) measured it: 3 of
            // 23 contract prompts exhausted a 4096 cap inside a
            // reasoning loop and returned no content at all.
            max_tokens: 8192,
        }
    }

    /// Baseten Muse Spark Open — the granted, verified route. The
    /// key works only with this exact model name.
    pub fn baseten(api_key: String) -> Self {
        OpenAiChatClient::new(
            "https://inference.baseten.co/v1",
            "meta-models/muse-spark-open-1026",
            api_key,
        )
    }

    /// Meta Model API — the sprint plan's primary route; use it once
    /// the $150 credit has landed in the account.
    pub fn meta(api_key: String) -> Self {
        OpenAiChatClient::new("https://api.meta.ai/v1", "muse-spark-1.3", api_key)
    }

    pub fn baseten_from_env() -> Option<Self> {
        std::env::var("BASETEN_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
            .map(OpenAiChatClient::baseten)
    }

    pub fn meta_from_env() -> Option<Self> {
        std::env::var("META_MODEL_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
            .map(OpenAiChatClient::meta)
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

impl ModelClient for OpenAiChatClient {
    fn parse_intent(&self, request: &ParseRequest) -> Result<String, AgentError> {
        let (system, user) = render_parse_messages(request);
        let body = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
            "max_tokens": self.max_tokens,
            "temperature": 0,
        });
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| {
                AgentError::ModelUnavailable(format!("could not build the HTTP client: {e}"))
            })?;
        let response = client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .map_err(|e| {
                AgentError::ModelUnavailable(format!(
                    "the model API ({}) did not answer: {e}",
                    self.base_url
                ))
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(AgentError::ModelUnavailable(format!(
                "the model API ({}) answered HTTP {status}",
                self.base_url
            )));
        }
        let payload: Value = response.json().map_err(|e| {
            AgentError::Model(format!("the model API answered something unreadable: {e}"))
        })?;
        payload
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                AgentError::Model("the model API reply had no message content".to_string())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{ParseInvoice, ParseItem};

    fn request(prompt: &str) -> ParseRequest {
        ParseRequest {
            prompt: prompt.to_string(),
            today: "2026-10-30".to_string(),
            customers: vec![
                "Acme".to_string(),
                "Beta".to_string(),
                "Delta".to_string(),
                "Epsilon".to_string(),
            ],
            items: vec![ParseItem {
                id: 1,
                sku: "BRK-100".to_string(),
                name: "Finished Bracket".to_string(),
                unit_price_cents: 12_500,
                standard_cost_cents: 6_800,
            }],
            open_invoices: vec![ParseInvoice {
                id: 1,
                customer_name: "Delta".to_string(),
                total_cents: 500_000,
                balance_cents: 500_000,
            }],
            suppliers: vec!["SteelWorks Supply".to_string()],
            open_purchase_orders: vec![],
            open_ap_invoices: vec![],
        }
    }

    #[test]
    fn stub_parses_the_contract_shapes() {
        let stub = StubClient;
        let intent = |p: &str| -> Value {
            serde_json::from_str(&stub.parse_intent(&request(p)).unwrap()).unwrap()
        };
        let t1 = intent("Invoice Acme for the 40 units we shipped Tuesday.");
        assert_eq!(t1["intent"], "invoice_shipment");
        assert_eq!(t1["customer"], "Acme");
        assert_eq!(t1["qty"], 40);
        assert_eq!(t1["day"], "Tuesday");

        let pay = intent("Delta paid $3,125 against their invoice.");
        assert_eq!(pay["intent"], "record_payment");
        assert_eq!(pay["customer"], "Delta");
        assert_eq!(pay["amount"], "$3,125");

        let dep = intent("Epsilon sent a $1,500 deposit for a future order.");
        assert_eq!(dep["intent"], "record_deposit");
        assert_eq!(dep["customer"], "Epsilon");

        let ret = intent("Delta returned 2 defective brackets.");
        assert_eq!(ret["intent"], "record_return");
        assert_eq!(ret["qty"], 2);

        let sup = intent("Pay $4,100 to the steel supplier against what we owe.");
        assert_eq!(sup["intent"], "pay_supplier");
        assert_eq!(sup["supplier"], "steel supplier");

        let prod = intent("We finished 50 brackets today, cost $4,100.");
        assert_eq!(prod["intent"], "complete_production");
        assert_eq!(prod["qty"], 50);
        assert_eq!(prod["amount"], "$4,100");

        let conflict = intent("Invoice Acme for 40 units at $130 each.");
        assert_eq!(conflict["price_each"], "$130");

        let missing = intent("Invoice Acme for the units we shipped.");
        assert_eq!(missing["intent"], "invoice_shipment");
        assert!(missing["qty"].is_null(), "no quantity may be invented");

        let trap = intent("Invoice Acme Manufacturing for 10 brackets.");
        assert_eq!(
            trap["customer"], "Acme Manufacturing",
            "the stub reports the name as said, so the adapter can clarify"
        );

        let po = intent("Create a purchase order for more steel.");
        assert_eq!(po["intent"], "out_of_scope");

        let recv = intent("Receive 60 FST-SET against purchase order 1.");
        assert_eq!(recv["intent"], "receive_goods");
        assert_eq!(recv["order_id"], 1);
        assert_eq!(recv["sku"], "FST-SET");
        assert_eq!(recv["qty"], 60);

        let ap = intent("Pay AP invoice 1.");
        assert_eq!(ap["intent"], "pay_ap_invoice");
        assert_eq!(ap["ap_invoice_id"], 1);
        assert!(ap["amount"].is_null(), "no amount stated — full balance");

        let held = intent("Pay AP invoice 1 anyway — the price increase was approved by Dana.");
        assert_eq!(held["intent"], "pay_ap_invoice");
        assert_eq!(held["override_reason"], "the price increase");
        assert_eq!(held["overrider"], "Dana");
    }

    #[test]
    fn live_client_posts_the_parse_request_and_reads_the_reply() {
        use std::io::{Read, Write};
        // A one-shot local HTTP server standing in for the model
        // API: capture the request, answer with a canned completion.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut content_length = 0usize;
            loop {
                let n = stream.read(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf);
                if let Some(headers_end) = text.find("\r\n\r\n") {
                    for line in text[..headers_end].lines() {
                        if let Some((name, value)) = line.split_once(':') {
                            if name.eq_ignore_ascii_case("content-length") {
                                content_length = value.trim().parse().unwrap_or(0);
                            }
                        }
                    }
                    if text.len() >= headers_end + 4 + content_length && content_length > 0 {
                        break;
                    }
                }
            }
            let request_text = String::from_utf8_lossy(&buf).to_string();
            let body = r#"{"choices":[{"message":{"content":"{\"intent\":\"record_rent\",\"amount\":\"$1,200\"}"}}]}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(reply.as_bytes()).unwrap();
            request_text
        });

        let client = OpenAiChatClient::new(
            &format!("http://127.0.0.1:{port}/v1"),
            "meta-models/muse-spark-open-1026",
            "test-key-not-a-credential".to_string(),
        );
        let reply = client
            .parse_intent(&request("Record this month's rent, $1,200."))
            .unwrap();
        assert!(reply.contains("record_rent"), "{reply}");

        let sent = server.join().unwrap();
        assert!(sent.starts_with("POST /v1/chat/completions"), "{sent}");
        assert!(
            sent.contains("authorization: Bearer test-key-not-a-credential"),
            "{sent}"
        );
        assert!(sent.contains("meta-models/muse-spark-open-1026"), "{sent}");
        assert!(
            sent.contains("Record this month's rent, $1,200."),
            "the verbatim prompt is in the request: {sent}"
        );
    }

    #[test]
    fn recorded_client_replays_and_misses_cleanly() {
        let client = RecordedClient::from_json(
            r#"{"exchanges":[{"prompt":"p1","response":{"intent":"record_rent","amount":"$5"}}]}"#,
        )
        .unwrap();
        let reply = client.parse_intent(&request("p1")).unwrap();
        assert!(reply.contains("record_rent"));
        let err = client.parse_intent(&request("other")).unwrap_err();
        assert!(matches!(err, AgentError::ModelUnavailable(_)));
    }

    #[test]
    fn fallback_chain_uses_the_recording_when_the_primary_is_down() {
        let client = FallbackClient::new(
            UnavailableClient {
                reason: "simulated outage".to_string(),
            },
            RecordedClient::from_json(
                r#"{"exchanges":[{"prompt":"p1","response":"{\"intent\":\"unclear\"}"}]}"#,
            )
            .unwrap(),
        );
        assert!(client.parse_intent(&request("p1")).is_ok());
        let err = client.parse_intent(&request("uncovered")).unwrap_err();
        match err {
            AgentError::ModelUnavailable(message) => {
                assert!(message.contains("erpcraft CLI"), "message was: {message}");
            }
            other => panic!("expected ModelUnavailable, got {other:?}"),
        }
    }
}
