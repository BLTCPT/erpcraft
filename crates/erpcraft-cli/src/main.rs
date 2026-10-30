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

use erpcraft_cli::commands::{
    all_specs, dispatch, open_door_db, spec_for, CommandSpec, DispatchError, Door,
};
use erpcraft_cli::mcp;
use serde_json::{json, Map, Value};
use std::process::ExitCode;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(|s| s.as_str()) {
        None | Some("help") | Some("--help") | Some("-h") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("mcp") => mcp::serve(),
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
        "contract": "JSON on stdout; plain-language refusals on stderr; exit 0 ok / 1 refused or invalid / 2 internal error. Database: ERPCRAFT_DB env (default erpcraft.db). 'erpcraft mcp' serves these same commands as MCP tools on stdio.",
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
/// the same detail the JSON surfaces carry, in terminal form.
fn report_error(e: &DispatchError) {
    eprintln!("{}", e.message);
    for explanation in &e.explanations {
        eprintln!("  - {}", explanation.message_parts.detail);
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
