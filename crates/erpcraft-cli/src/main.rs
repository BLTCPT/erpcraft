//! `erpcraft` — the operator CLI door over the dispatch table in
//! `erpcraft-cli`'s library (see `commands.rs`). One JSON document on
//! stdout per command; refusals in plain language on stderr; exit
//! 0 = ok, 1 = refused or invalid, 2 = internal error. `erpcraft mcp`
//! serves the same table as an MCP server on stdio.
//!
//! Arguments: `--json '<object>'` for the full argument object, and/or
//! `--flag value` pairs (hyphens become underscores), and/or one bare
//! positional value for the command's primary argument (an id, or the
//! account code for `account_ledger`).

use erpcraft_cli::agent::clients::{
    FallbackClient, ModelClient, OpenAiChatClient, RecordedClient, StubClient,
};
use erpcraft_cli::agent::{today_utc, Agent, Proposal, ProposeOutcome};
use erpcraft_cli::commands::{
    all_specs, dispatch, open_door_db, spec_for, CommandSpec, DispatchError, Door,
};
use erpcraft_cli::mcp;
use serde_json::{json, Map, Value};
use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(|s| s.as_str()) {
        None | Some("help") | Some("--help") | Some("-h") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("mcp") => mcp::serve(),
        Some("agent") => run_agent(&argv[1..]),
        Some(name) => run(name, &argv[1..]),
    }
}

fn print_help() {
    let commands: Vec<Value> = all_specs()
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "usage": s.usage,
                "summary": s.summary,
            })
        })
        .collect();
    let doc = json!({
        "name": "erpcraft",
        "contract": "JSON on stdout; plain-language refusals on stderr; exit 0 ok / 1 refused or invalid / 2 internal error. Database: ERPCRAFT_DB env (default erpcraft.db). 'erpcraft mcp' serves these same commands as MCP tools on stdio. 'erpcraft agent \"<prompt>\"' runs the natural-language agent over this table (propose → confirm → post; --client stub|recorded|baseten|meta).",
        "commands": commands,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
    );
}

fn run(name: &str, tokens: &[String]) -> ExitCode {
    let Some(spec) = spec_for(name) else {
        eprintln!("unknown command '{name}' — run 'erpcraft help' to list the commands");
        return ExitCode::from(1);
    };
    if tokens.iter().any(|t| t == "--help" || t == "-h") {
        println!("usage: {}", spec.usage);
        println!("{}", spec.summary);
        return ExitCode::SUCCESS;
    }
    let args = match parse_cli_args(&spec, tokens) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("usage: {}", spec.usage);
            return ExitCode::from(1);
        }
    };
    let mut conn = match open_door_db() {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("{}", e.message);
            return ExitCode::from(2);
        }
    };
    match dispatch(&mut conn, Door::Cli, name, &args) {
        Ok(value) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string())
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            report_error(&e);
            if e.internal {
                ExitCode::from(2)
            } else {
                ExitCode::from(1)
            }
        }
    }
}

/// A refusal in plain language: the core's message, then each of the
/// configurator's structured explanations rendered as a reason line —
/// the same detail the JSON surfaces carry, in terminal form. Each
/// distinct reason prints once: the message line often already
/// carries the first explanation's detail (evaluate_config's
/// contract), and one dependency can explain several values with the
/// same sentence.
fn report_error(e: &DispatchError) {
    eprintln!("{}", e.message);
    let mut seen: Vec<&str> = Vec::new();
    for explanation in &e.explanations {
        let detail = explanation.message_parts.detail.as_str();
        if e.message.contains(detail) || seen.contains(&detail) {
            continue;
        }
        seen.push(detail);
        eprintln!("  - {detail}");
    }
}

/// Translate CLI tokens into the command's JSON argument object:
/// `--json` payloads merge first, then `--flag value` pairs, then one
/// bare positional mapped to the spec's primary argument.
fn parse_cli_args(spec: &CommandSpec, tokens: &[String]) -> Result<Value, String> {
    let mut obj = Map::new();
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        if token == "--json" {
            i += 1;
            let raw = tokens
                .get(i)
                .ok_or("--json needs a JSON object value after it")?;
            merge_json(&mut obj, raw)?;
        } else if let Some(raw) = token.strip_prefix("--json=") {
            merge_json(&mut obj, raw)?;
        } else if let Some(flag) = token.strip_prefix("--") {
            let (key, inline) = match flag.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (flag.to_string(), None),
            };
            let raw = match inline {
                Some(v) => v,
                None => {
                    i += 1;
                    tokens
                        .get(i)
                        .ok_or_else(|| format!("--{key} needs a value after it"))?
                        .clone()
                }
            };
            obj.insert(key.replace('-', "_"), parse_scalar(&raw));
        } else {
            positional.push(token.clone());
        }
        i += 1;
    }
    if !positional.is_empty() {
        let Some(primary) = spec.primary_arg else {
            return Err(format!(
                "'{}' takes no positional argument — pass arguments as --flag value or --json",
                spec.name
            ));
        };
        if positional.len() > 1 {
            return Err("too many positional arguments".to_string());
        }
        if obj.contains_key(primary) {
            return Err(format!(
                "argument '{primary}' was given twice (positionally and by flag)"
            ));
        }
        obj.insert(primary.to_string(), parse_scalar(&positional[0]));
    }
    Ok(Value::Object(obj))
}

fn merge_json(obj: &mut Map<String, Value>, raw: &str) -> Result<(), String> {
    let parsed: Value =
        serde_json::from_str(raw).map_err(|e| format!("--json value is not valid JSON: {e}"))?;
    let Value::Object(map) = parsed else {
        return Err("--json value must be a JSON object".to_string());
    };
    for (k, v) in map {
        obj.insert(k, v);
    }
    Ok(())
}

/// A flag value: numbers, booleans and quoted strings parse as their
/// JSON types; anything else is a bare string (account codes, dates).
fn parse_scalar(raw: &str) -> Value {
    match serde_json::from_str::<Value>(raw) {
        Ok(v @ (Value::Number(_) | Value::Bool(_))) => v,
        Ok(Value::String(s)) => Value::String(s),
        _ => Value::String(raw.to_string()),
    }
}

/// The Day-5 agent door: `erpcraft agent [--client stub|recorded|baseten|meta]
/// [--fixture <path>] [--yes] "<prompt>"`.
///
/// The agent proposes; the operator confirms at the prompt (or with
/// an explicit `--yes`, which still prints the confirmation first);
/// only then does anything post. Clients: `stub` (deterministic,
/// offline — the default), `recorded` (replay a captured exchange
/// from --fixture / ERPCRAFT_AGENT_FIXTURE), `baseten` and `meta`
/// (live; key from BASETEN_API_KEY / META_MODEL_API_KEY). The live
/// routes fall back to the recorded fixture when one is configured
/// and the API does not answer (spec test 12).
fn run_agent(tokens: &[String]) -> ExitCode {
    if tokens.iter().any(|t| t == "--help" || t == "-h") {
        println!(
            "usage: erpcraft agent [--client stub|recorded|baseten|meta] [--fixture <path>] [--yes] \"<prompt>\""
        );
        println!(
            "The natural-language agent: turns a plain-English prompt into proposed dispatch calls, \
             renders the confirmation from ledger data, and posts only after a human confirms."
        );
        return ExitCode::SUCCESS;
    }
    let mut client_kind = "stub".to_string();
    let mut fixture: Option<String> = std::env::var("ERPCRAFT_AGENT_FIXTURE").ok();
    let mut auto_yes = false;
    let mut print_request = false;
    let mut words: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i].as_str() {
            "--client" => {
                i += 1;
                match tokens.get(i) {
                    Some(v) => client_kind = v.clone(),
                    None => {
                        eprintln!("--client needs a value: stub, recorded, baseten or meta");
                        return ExitCode::from(1);
                    }
                }
            }
            "--fixture" => {
                i += 1;
                match tokens.get(i) {
                    Some(v) => fixture = Some(v.clone()),
                    None => {
                        eprintln!("--fixture needs a path to a recorded-response fixture");
                        return ExitCode::from(1);
                    }
                }
            }
            "--yes" => auto_yes = true,
            "--print-parse-request" => print_request = true,
            other => words.push(other.to_string()),
        }
        i += 1;
    }
    let prompt = words.join(" ");
    if prompt.trim().is_empty() {
        eprintln!(
            "usage: erpcraft agent [--client stub|recorded|baseten|meta] [--fixture <path>] [--yes] \"<prompt>\""
        );
        return ExitCode::from(1);
    }
    let mut conn = match open_door_db() {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("{}", e.message);
            return ExitCode::from(2);
        }
    };

    if print_request {
        // The exact messages the live routes send — used to record a
        // genuine exchange for the demo fallback fixture.
        let request = match agent_parse_request(&mut conn, &prompt) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(2);
            }
        };
        let (system, user) = erpcraft_cli::agent::clients::render_parse_messages(&request);
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "prompt": prompt,
                "system": system,
                "user": user,
            }))
            .unwrap_or_else(|_| "{}".to_string())
        );
        return ExitCode::SUCCESS;
    }

    let recorded = match &fixture {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => match RecordedClient::from_json(&text) {
                Ok(client) => Some(client),
                Err(e) => {
                    eprintln!("recorded fixture {path}: {e}");
                    return ExitCode::from(2);
                }
            },
            Err(e) => {
                eprintln!("cannot read recorded fixture {path}: {e}");
                return ExitCode::from(2);
            }
        },
        None => None,
    };

    // Box the chosen client stack behind the trait.
    let client: Box<dyn ModelClient> = match client_kind.as_str() {
        "stub" => Box::new(StubClient),
        "recorded" => match recorded {
            Some(client) => Box::new(client),
            None => {
                eprintln!(
                    "--client recorded needs a fixture (--fixture <path> or ERPCRAFT_AGENT_FIXTURE)"
                );
                return ExitCode::from(1);
            }
        },
        "baseten" | "meta" => {
            let live = if client_kind == "baseten" {
                OpenAiChatClient::baseten_from_env()
            } else {
                OpenAiChatClient::meta_from_env()
            };
            match (live, recorded) {
                (Some(live), Some(rec)) => Box::new(FallbackClient::new(live, rec)),
                (Some(live), None) => Box::new(live),
                (None, _) => {
                    let var = if client_kind == "baseten" {
                        "BASETEN_API_KEY"
                    } else {
                        "META_MODEL_API_KEY"
                    };
                    eprintln!(
                        "{var} is not set — the live {client_kind} route needs the operator's key in the environment"
                    );
                    return ExitCode::from(2);
                }
            }
        }
        other => {
            eprintln!("unknown --client '{other}': stub, recorded, baseten or meta");
            return ExitCode::from(1);
        }
    };

    let agent = Agent::new(client, today_utc());
    match agent.propose(&mut conn, &prompt) {
        Ok(ProposeOutcome::NeedsConfirmation(proposal)) => {
            confirm_and_post(&mut conn, &proposal, auto_yes)
        }
        Ok(ProposeOutcome::Clarification { question, .. }) => {
            println!("{question}");
            ExitCode::SUCCESS
        }
        Ok(ProposeOutcome::Refusal { message, .. })
        | Ok(ProposeOutcome::OutOfScope { message, .. }) => {
            println!("{message}");
            ExitCode::from(1)
        }
        Ok(ProposeOutcome::PriceConflict { question, .. }) => {
            println!("{question}");
            ExitCode::SUCCESS
        }
        Ok(ProposeOutcome::DuplicateWarning { warning, .. }) => {
            println!("{warning}");
            ExitCode::SUCCESS
        }
        Ok(ProposeOutcome::Answer { message, .. }) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
    }
}

/// Print the confirmation, take the human's answer, and only then
/// post. This is the contract's gate, at the terminal.
fn confirm_and_post(
    conn: &mut erpcraft_core::rusqlite::Connection,
    proposal: &Proposal,
    auto_yes: bool,
) -> ExitCode {
    if let Some(warning) = &proposal.duplicate_warning {
        println!("{warning}");
    }
    println!("{}", proposal.confirmation_text);
    let confirmed = if auto_yes {
        println!("(--yes given — posting.)");
        true
    } else {
        print!("Confirm? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let answered = match std::io::stdin().read_line(&mut line) {
            Ok(_) => matches!(line.trim().to_lowercase().as_str(), "y" | "yes"),
            Err(_) => false,
        };
        // Terminate the prompt line. At a terminal the human's Enter
        // does this; with piped stdin nothing would, and the result
        // JSON (or the cancel message) would start on the prompt line.
        println!();
        answered
    };
    if !confirmed {
        println!("{}", proposal.cancel_message());
        return ExitCode::SUCCESS;
    }
    match proposal.confirm(conn) {
        Ok(result) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string())
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

/// Gather the parse-request context through the dispatch reads, the
/// same numbers the adapter itself uses.
fn agent_parse_request(
    conn: &mut erpcraft_core::rusqlite::Connection,
    prompt: &str,
) -> Result<erpcraft_cli::agent::ParseRequest, String> {
    let mut read = |name: &str| -> Result<Value, String> {
        dispatch(conn, Door::Cli, name, &json!({})).map_err(|e| e.message)
    };
    let customers = read("list_customers")?;
    let items = read("list_items")?;
    let invoices = read("list_invoices")?;
    let suppliers = read("list_suppliers")?;
    let purchase_orders = read("list_procurement_orders")?;
    let ap_invoices = read("list_ap_invoices")?;
    let arr = |v: &Value| v.as_array().cloned().unwrap_or_default();
    let gi = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let gs = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    Ok(erpcraft_cli::agent::ParseRequest {
        prompt: prompt.to_string(),
        today: today_utc(),
        customers: arr(&customers).iter().map(|c| gs(c, "name")).collect(),
        items: arr(&items)
            .iter()
            .map(|i| erpcraft_cli::agent::ParseItem {
                id: gi(i, "id"),
                sku: gs(i, "sku"),
                name: gs(i, "name"),
                unit_price_cents: gi(i, "unit_price_cents"),
                standard_cost_cents: gi(i, "standard_cost_cents"),
            })
            .collect(),
        open_invoices: arr(&invoices)
            .iter()
            .filter(|inv| {
                matches!(gs(inv, "status").as_str(), "issued" | "partially_paid")
                    && gi(inv, "balance_cents") > 0
            })
            .map(|inv| erpcraft_cli::agent::ParseInvoice {
                id: gi(inv, "id"),
                customer_name: gs(inv, "customer_name"),
                total_cents: gi(inv, "total_cents"),
                balance_cents: gi(inv, "balance_cents"),
            })
            .collect(),
        suppliers: arr(&suppliers).iter().map(|s| gs(s, "name")).collect(),
        open_purchase_orders: arr(&purchase_orders)
            .iter()
            .filter(|po| matches!(gs(po, "status").as_str(), "open" | "partially_received"))
            .map(|po| erpcraft_cli::agent::ParsePo {
                id: gi(po, "id"),
                supplier_name: gs(po, "supplier_name"),
                total_cents: gi(po, "total_cents"),
                received_cents: gi(po, "received_cents"),
            })
            .collect(),
        open_ap_invoices: arr(&ap_invoices)
            .iter()
            .filter(|inv| {
                matches!(
                    gs(inv, "status").as_str(),
                    "open" | "partially_paid" | "on_hold"
                ) && gi(inv, "balance_cents") > 0
            })
            .map(|inv| erpcraft_cli::agent::ParseApInvoice {
                id: gi(inv, "id"),
                supplier_name: gs(inv, "supplier_name"),
                total_cents: gi(inv, "total_cents"),
                balance_cents: gi(inv, "balance_cents"),
                status: gs(inv, "status"),
            })
            .collect(),
    })
}
