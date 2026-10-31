//! Model clients for the Day-5 agent (DAY5_PREP.md): the trait, the
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

Return exactly this shape, omitting fields that do not apply:
{\"intent\": \"...\", \"customer\": \"...\", \"supplier\": \"...\", \"qty\": 0, \
\"amount\": \"...\", \"price_each\": \"...\", \"day\": \"...\", \"topic\": \"...\"}

Intents: invoice_shipment, record_payment, buy_materials, pay_supplier, \
issue_materials, record_labor, complete_production, record_deposit, \
record_return, record_rent, record_interest, record_depreciation. Anything \
else (purchase orders, payroll, tax, multi-currency) is \
{\"intent\": \"out_of_scope\", \"topic\": \"...\"}. If you cannot tell, \
{\"intent\": \"unclear\"}.

Rules:
- Copy customer names and money exactly as written in the prompt (\"$3,125\" \
stays \"$3,125\"). Never convert money to a number, never round it.
- qty is the unit count as an integer, when the prompt states one.
- price_each only when the prompt states a per-unit price (\"at $130 each\").
- day is the weekday name as written (\"Tuesday\") or \"today\"."
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
    let user = format!(
        "Today is {}.\nCustomers on file: {customers}\nItems: {items}\nOpen invoices: {invoices}\nPrompt: {}",
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
        Ok(stub_parse(&request.prompt).to_string())
    }
}

/// The stub's parse of one prompt into the intent JSON shape.
pub fn stub_parse(prompt: &str) -> Value {
    let lower = prompt.to_lowercase();
    let amount = first_money(prompt);
    let qty = stated_qty(prompt);
    let day = stated_day(prompt);

    if lower.contains("purchase order") {
        return json!({ "intent": "out_of_scope", "topic": "purchase orders" });
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
            max_tokens: 4096,
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
