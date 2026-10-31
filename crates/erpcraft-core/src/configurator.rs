//! Variant configurator — SAP LO-VC's dependency model, rebuilt as typed
//! data (see `CONFIGURATOR_DESIGN.md` at the repository root).
//!
//! Three dependency kinds are stored as typed expression trees — never as
//! source text to be parsed:
//!
//! - **Precondition** — gates which characteristics / values are in play.
//! - **Selection condition** — when true, the target value or component is
//!   selected. Selections are what turn a configuration into a concrete
//!   component list.
//! - **Constraint** — cross-characteristic consistency. When violated, the
//!   configuration is inconsistent and the evaluator reports exactly which
//!   constraint fired on which values.
//!
//! [`evaluate`] is a pure, deterministic function over a loaded
//! [`ConfigModelView`]: same inputs, same output, no database writes, no
//! AI inside. Pricing is *not* part of evaluation — the quote layer
//! consumes the evaluator's selected components and rolls their deltas
//! into the line price (in `lib.rs`).

use crate::{Cents, LedgerError, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Expression language — a typed AST, never parsed from strings.
// ---------------------------------------------------------------------------

/// Comparison operators for [`Expr::Cmp`] (integer characteristics only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CmpOp {
    #[serde(rename = "<")]
    Lt,
    #[serde(rename = "<=")]
    Le,
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = ">=")]
    Ge,
    #[serde(rename = "==")]
    Eq,
    #[serde(rename = "!=")]
    Ne,
}

impl CmpOp {
    fn apply(self, lhs: i64, rhs: i64) -> bool {
        match self {
            CmpOp::Lt => lhs < rhs,
            CmpOp::Le => lhs <= rhs,
            CmpOp::Gt => lhs > rhs,
            CmpOp::Ge => lhs >= rhs,
            CmpOp::Eq => lhs == rhs,
            CmpOp::Ne => lhs != rhs,
        }
    }
}

/// One node of a dependency expression. Deliberately inexpressive: no
/// arithmetic beyond integer comparison, no references to documents,
/// money, or inventory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Expr {
    /// The characteristic equals the value (string code for enum/bool
    /// characteristics is *not* used here — the JSON value must match the
    /// characteristic's type: string for enum, number for integer,
    /// boolean for bool).
    Eq {
        #[serde(rename = "char")]
        char_code: String,
        value: serde_json::Value,
    },
    /// The characteristic's value is one of the listed values.
    In {
        #[serde(rename = "char")]
        char_code: String,
        values: Vec<serde_json::Value>,
    },
    /// Integer comparison against the characteristic's value.
    Cmp {
        #[serde(rename = "char")]
        char_code: String,
        op: CmpOp,
        value: i64,
    },
    And {
        all: Vec<Expr>,
    },
    Or {
        any: Vec<Expr>,
    },
    Not {
        expr: Box<Expr>,
    },
}

impl Expr {
    /// Characteristics referenced by this expression, in first-use order.
    fn referenced_chars(&self, out: &mut Vec<String>) {
        match self {
            Expr::Eq { char_code, .. }
            | Expr::In { char_code, .. }
            | Expr::Cmp { char_code, .. } => {
                if !out.contains(char_code) {
                    out.push(char_code.clone());
                }
            }
            Expr::And { all } => {
                for e in all {
                    e.referenced_chars(out);
                }
            }
            Expr::Or { any } => {
                for e in any {
                    e.referenced_chars(out);
                }
            }
            Expr::Not { expr } => expr.referenced_chars(out),
        }
    }

    fn chars(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.referenced_chars(&mut out);
        out
    }
}

/// A value assigned to a characteristic in a (partial) configuration.
/// Serializes as the bare JSON value: string for enum codes, number for
/// integers, boolean for bools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConfigValue {
    Int(i64),
    Bool(bool),
    Code(String),
}

impl ConfigValue {
    /// Canonical string rendering, used in `selectable` lists and
    /// explanation parts.
    pub fn render(&self) -> String {
        match self {
            ConfigValue::Int(i) => i.to_string(),
            ConfigValue::Bool(b) => b.to_string(),
            ConfigValue::Code(c) => c.clone(),
        }
    }
}

/// Does a configuration value match an expression's JSON value?
fn json_matches(value: &ConfigValue, json: &serde_json::Value) -> bool {
    match (value, json) {
        (ConfigValue::Code(c), serde_json::Value::String(s)) => c == s,
        (ConfigValue::Int(i), serde_json::Value::Number(n)) => n.as_i64() == Some(*i),
        (ConfigValue::Bool(b), serde_json::Value::Bool(j)) => b == j,
        _ => false,
    }
}

/// Tri-state expression evaluation: `None` means "cannot decide yet"
/// because a referenced characteristic is unassigned. Adding assignments
/// is monotone — a decided result never flips.
fn eval_expr(expr: &Expr, config: &BTreeMap<String, ConfigValue>) -> Option<bool> {
    match expr {
        Expr::Eq { char_code, value } => config.get(char_code).map(|v| json_matches(v, value)),
        Expr::In { char_code, values } => config
            .get(char_code)
            .map(|v| values.iter().any(|j| json_matches(v, j))),
        Expr::Cmp {
            char_code,
            op,
            value,
        } => match config.get(char_code) {
            Some(ConfigValue::Int(i)) => Some(op.apply(*i, *value)),
            Some(_) => Some(false),
            None => None,
        },
        Expr::And { all } => {
            let mut unknown = false;
            for e in all {
                match eval_expr(e, config) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            if unknown {
                None
            } else {
                Some(true)
            }
        }
        Expr::Or { any } => {
            let mut unknown = false;
            for e in any {
                match eval_expr(e, config) {
                    Some(true) => return Some(true),
                    None => unknown = true,
                    Some(false) => {}
                }
            }
            if unknown {
                None
            } else {
                Some(false)
            }
        }
        Expr::Not { expr } => eval_expr(expr, config).map(|b| !b),
    }
}

// ---------------------------------------------------------------------------
// Data transfer types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CharacteristicValueView {
    pub id: i64,
    pub code: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CharacteristicView {
    pub id: i64,
    pub code: String,
    pub name: String,
    /// "enum" | "integer" | "bool"
    pub value_type: String,
    /// Integer characteristics only.
    pub min_value: Option<i64>,
    pub max_value: Option<i64>,
    pub required: bool,
    /// The enum domain, in definition order (empty for integer/bool).
    pub values: Vec<CharacteristicValueView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentView {
    pub id: i64,
    /// Targeted by selection dependencies as `component:<code>`.
    pub code: String,
    pub name: String,
    /// The stocked item this component corresponds to, when it is one.
    /// The demo components (blanks, finish treatments, mount kit) are not
    /// sellable items, so the seed leaves this null — see the design doc's
    /// data model; the column is there for models whose components are.
    pub component_item_id: Option<i64>,
    /// Component quantity per unit of the configured item.
    pub qty: i64,
    /// Rolled into a configured line's unit price when selected.
    pub price_delta_cents: Cents,
    /// Rolled into a configured line's standard cost when selected.
    pub cost_delta_cents: Cents,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyView {
    pub id: i64,
    pub model_id: i64,
    /// "precondition" | "selection" | "constraint"
    pub kind: String,
    /// What the dependency governs. Precondition: a characteristic code
    /// (`finish`) or one value (`finish=galvanized`). Selection: a value
    /// (`mounting=reinforced`) or a component (`component:steel-blank`).
    /// Constraint: a free label naming the rule.
    pub target: String,
    pub expr: Expr,
    /// The human sentence the dependency was authored from. Provenance
    /// for the audit trail and explanations — never parsed.
    pub source_text: String,
    pub created_by: String,
}

/// A fully loaded configuration model: the evaluator's input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigModelView {
    pub id: i64,
    pub item_id: i64,
    pub item_sku: String,
    pub name: String,
    pub characteristics: Vec<CharacteristicView>,
    pub components: Vec<ComponentView>,
    pub dependencies: Vec<DependencyView>,
}

impl ConfigModelView {
    pub fn characteristic(&self, code: &str) -> Option<&CharacteristicView> {
        self.characteristics.iter().find(|c| c.code == code)
    }

    pub fn component(&self, code: &str) -> Option<&ComponentView> {
        self.components.iter().find(|c| c.code == code)
    }

    fn dependencies_of_kind(&self, kind: &str) -> Vec<&DependencyView> {
        self.dependencies
            .iter()
            .filter(|d| d.kind == kind)
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewConfigModel {
    pub item_id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewCharacteristicValue {
    pub code: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewCharacteristic {
    pub code: String,
    pub name: String,
    /// "enum" | "integer" | "bool"
    pub value_type: String,
    #[serde(default)]
    pub min_value: Option<i64>,
    #[serde(default)]
    pub max_value: Option<i64>,
    #[serde(default = "default_true")]
    pub required: bool,
    /// The enum domain (required for enum characteristics, ignored
    /// otherwise).
    #[serde(default)]
    pub values: Vec<NewCharacteristicValue>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewComponent {
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub component_item_id: Option<i64>,
    #[serde(default = "default_one")]
    pub qty: i64,
    pub price_delta_cents: Cents,
    pub cost_delta_cents: Cents,
}

fn default_one() -> i64 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewDependency {
    /// "precondition" | "selection" | "constraint"
    pub kind: String,
    pub target: String,
    pub expr: Expr,
    #[serde(default)]
    pub source_text: String,
    #[serde(default = "default_created_by")]
    pub created_by: String,
}

fn default_created_by() -> String {
    "api".to_string()
}

// ---------------------------------------------------------------------------
// Evaluation — the deterministic core of the configurator.
// ---------------------------------------------------------------------------

/// Structured parts of an explanation: which characteristics and values
/// were involved, plus a human-readable detail (the dependency's
/// `source_text` when it has one). Renderers on every surface (HTTP,
/// CLI, MCP, agent) render the *same* reason from these parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageParts {
    pub characteristics: Vec<String>,
    pub values: Vec<String>,
    pub detail: String,
}

/// One reason something happened during evaluation: a dependency fired,
/// a chosen value conflicts, a constraint was violated. A rejection
/// without an explanation is a bug.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Explanation {
    /// The dependency responsible, when one is. `None` for input
    /// validation problems, which no dependency governs.
    pub dependency_id: Option<i64>,
    /// "precondition" | "selection" | "constraint" | "validation"
    pub kind: String,
    pub message_parts: MessageParts,
}

/// A characteristic value the evaluator forced, and the dependency
/// that forced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForcedValue {
    pub value: ConfigValue,
    pub dependency_id: i64,
    /// "selection" | "constraint" | "precondition"
    pub kind: String,
}

/// A component selected by a selection condition (components with no
/// selection dependency are baseline selections, not forced ones).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForcedComponent {
    pub component: String,
    pub dependency_id: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Verdict {
    Consistent,
    /// Required characteristics still unassigned.
    Incomplete {
        missing: Vec<String>,
    },
    Inconsistent,
}

/// The full result of evaluating a (partial) configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evaluation {
    pub verdict: Verdict,
    /// Per characteristic, the values still allowed (enum/bool domains;
    /// integer characteristics cannot be enumerated and map to an
    /// empty list — their range lives on the characteristic itself).
    pub selectable: BTreeMap<String, Vec<String>>,
    /// Values the evaluator forced, each naming its dependency.
    pub forced: BTreeMap<String, ForcedValue>,
    /// Components selected by a fired selection condition.
    pub forced_components: Vec<ForcedComponent>,
    /// All selected component codes (baseline + forced), in component
    /// definition order. The pricing step consumes this list.
    pub selected_components: Vec<String>,
    pub explanations: Vec<Explanation>,
}

// ---------------------------------------------------------------------------
// Dependency targets
// ---------------------------------------------------------------------------

/// A parsed dependency target.
enum Target {
    /// `finish` — the whole characteristic (preconditions).
    Char(String),
    /// `finish=galvanized` — one value of a characteristic.
    Value(String, ConfigValue),
    /// `component:steel-blank` — a component (selections).
    Component(String),
}

/// Convert a target's value string to a typed value using the
/// characteristic's type. Save-time validation guarantees this parses
/// for stored dependencies; the fallback keeps evaluation total.
fn target_value(ch: &CharacteristicView, raw: &str) -> ConfigValue {
    match ch.value_type.as_str() {
        "integer" => raw
            .parse::<i64>()
            .map(ConfigValue::Int)
            .unwrap_or_else(|_| ConfigValue::Code(raw.to_string())),
        "bool" => match raw {
            "true" => ConfigValue::Bool(true),
            "false" => ConfigValue::Bool(false),
            _ => ConfigValue::Code(raw.to_string()),
        },
        _ => ConfigValue::Code(raw.to_string()),
    }
}

fn parse_target(model: &ConfigModelView, target: &str) -> Option<Target> {
    if let Some(code) = target.strip_prefix("component:") {
        return Some(Target::Component(code.to_string()));
    }
    match target.split_once('=') {
        Some((code, raw)) => {
            let ch = model.characteristic(code)?;
            Some(Target::Value(code.to_string(), target_value(ch, raw)))
        }
        None => Some(Target::Char(target.to_string())),
    }
}

/// If a selection dependency forces a characteristic value, return
/// (characteristic code, forced value).
fn selection_char_target(
    model: &ConfigModelView,
    dep: &DependencyView,
) -> Option<(String, ConfigValue)> {
    match parse_target(model, &dep.target) {
        Some(Target::Value(code, value)) => Some((code, value)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Value domains and candidate filtering
// ---------------------------------------------------------------------------

/// Validate one assigned value against its characteristic's domain.
fn validate_value(ch: &CharacteristicView, value: &ConfigValue) -> std::result::Result<(), String> {
    match ch.value_type.as_str() {
        "enum" => match value {
            ConfigValue::Code(code) if ch.values.iter().any(|v| &v.code == code) => Ok(()),
            _ => Err(format!(
                "characteristic '{}' expects one of its values, got '{}'",
                ch.code,
                value.render()
            )),
        },
        "integer" => match value {
            ConfigValue::Int(i) => {
                if let Some(min) = ch.min_value {
                    if *i < min {
                        return Err(format!(
                            "characteristic '{}' must be at least {min}, got {i}",
                            ch.code
                        ));
                    }
                }
                if let Some(max) = ch.max_value {
                    if *i > max {
                        return Err(format!(
                            "characteristic '{}' must be at most {max}, got {i}",
                            ch.code
                        ));
                    }
                }
                Ok(())
            }
            _ => Err(format!(
                "characteristic '{}' expects an integer, got '{}'",
                ch.code,
                value.render()
            )),
        },
        "bool" => match value {
            ConfigValue::Bool(_) => Ok(()),
            _ => Err(format!(
                "characteristic '{}' expects true or false, got '{}'",
                ch.code,
                value.render()
            )),
        },
        other => Err(format!(
            "characteristic '{}' has unknown value type '{other}'",
            ch.code
        )),
    }
}

/// The finite domain of a characteristic (empty for integers, whose
/// domain is a range, not a list).
fn domain_values(ch: &CharacteristicView) -> Vec<ConfigValue> {
    match ch.value_type.as_str() {
        "enum" => ch
            .values
            .iter()
            .map(|v| ConfigValue::Code(v.code.clone()))
            .collect(),
        "bool" => vec![ConfigValue::Bool(true), ConfigValue::Bool(false)],
        _ => Vec::new(),
    }
}

/// Precondition dependencies governing a characteristic as a whole or
/// one of its values, in dependency-id order.
fn preconditions_for<'m>(
    model: &'m ConfigModelView,
    ch: &CharacteristicView,
    only_value: Option<&ConfigValue>,
) -> Vec<&'m DependencyView> {
    model
        .dependencies_of_kind("precondition")
        .into_iter()
        .filter(|dep| match parse_target(model, &dep.target) {
            Some(Target::Char(code)) => code == ch.code,
            Some(Target::Value(code, value)) => {
                code == ch.code && only_value.map(|v| *v == value).unwrap_or(false)
            }
            _ => false,
        })
        .collect()
}

/// The dependency (if any) that blocks assigning `value` to `ch` given
/// the current effective assignments. Checks, in a fixed order: the
/// value's own preconditions, the preconditions of already-chosen
/// values, selection conditions that would force a different value,
/// and constraints. Returning the first blocker keeps the reported
/// "responsible dependency" deterministic.
fn candidate_blocker(
    model: &ConfigModelView,
    ch: &CharacteristicView,
    value: &ConfigValue,
    effective: &BTreeMap<String, ConfigValue>,
) -> Option<i64> {
    let mut tentative = effective.clone();
    tentative.insert(ch.code.clone(), value.clone());

    // 1. The candidate's own preconditions must not be definitely false.
    for dep in preconditions_for(model, ch, Some(value)) {
        if eval_expr(&dep.expr, &tentative) == Some(false) {
            return Some(dep.id);
        }
    }
    // 2. Choosing this value must not break a precondition of a value
    //    already chosen for another characteristic.
    for (other_code, other_value) in effective.iter() {
        if other_code == &ch.code {
            continue;
        }
        let Some(other) = model.characteristic(other_code) else {
            continue;
        };
        for dep in preconditions_for(model, other, Some(other_value)) {
            if eval_expr(&dep.expr, &tentative) == Some(false) {
                return Some(dep.id);
            }
        }
    }
    // 3. A selection condition already true must not force a different
    //    value for this characteristic.
    for dep in model.dependencies_of_kind("selection") {
        if eval_expr(&dep.expr, &tentative) != Some(true) {
            continue;
        }
        if let Some((code, want)) = selection_char_target(model, dep) {
            if code == ch.code && want != *value {
                return Some(dep.id);
            }
        }
    }
    // 4. No constraint may become definitely false.
    for dep in model.dependencies_of_kind("constraint") {
        if eval_expr(&dep.expr, &tentative) == Some(false) {
            return Some(dep.id);
        }
    }
    None
}

/// The values of `ch` that remain allowed given `effective` (which must
/// not assign `ch` itself), plus the values that were eliminated and
/// the dependency that eliminated each.
fn allowed_values(
    model: &ConfigModelView,
    ch: &CharacteristicView,
    effective: &BTreeMap<String, ConfigValue>,
) -> (Vec<ConfigValue>, Vec<(ConfigValue, i64)>) {
    let mut allowed = Vec::new();
    let mut eliminated = Vec::new();
    for value in domain_values(ch) {
        match candidate_blocker(model, ch, &value, effective) {
            None => allowed.push(value),
            Some(dep_id) => eliminated.push((value, dep_id)),
        }
    }
    (allowed, eliminated)
}

/// The char-level precondition (if any) that takes a whole
/// characteristic out of play under the current assignments.
fn out_of_play_dep(
    model: &ConfigModelView,
    ch: &CharacteristicView,
    effective: &BTreeMap<String, ConfigValue>,
) -> Option<i64> {
    for dep in model.dependencies_of_kind("precondition") {
        if let Some(Target::Char(code)) = parse_target(model, &dep.target) {
            if code == ch.code && eval_expr(&dep.expr, effective) == Some(false) {
                return Some(dep.id);
            }
        }
    }
    None
}

fn explanation(
    dep: &DependencyView,
    characteristics: Vec<String>,
    values: Vec<String>,
    fallback_detail: String,
) -> Explanation {
    Explanation {
        dependency_id: Some(dep.id),
        kind: dep.kind.clone(),
        message_parts: MessageParts {
            characteristics,
            values,
            detail: if dep.source_text.is_empty() {
                fallback_detail
            } else {
                dep.source_text.clone()
            },
        },
    }
}

/// Render the assignments of the given characteristics, for
/// explanation parts ("quantity=50").
fn assignment_parts(effective: &BTreeMap<String, ConfigValue>, chars: &[String]) -> Vec<String> {
    chars
        .iter()
        .filter_map(|c| effective.get(c).map(|v| format!("{c}={}", v.render())))
        .collect()
}

// ---------------------------------------------------------------------------
// The evaluator
// ---------------------------------------------------------------------------

/// Evaluate a (partial) configuration against a loaded model.
///
/// Pure and deterministic: no database access, no writes, same inputs
/// produce a byte-identical [`Evaluation`] when serialized. Phases, per
/// the design doc:
///
/// 1. **Prune** — preconditions remove out-of-play characteristics and
///    values from the selectable space. A chosen value whose
///    precondition fails is a reported conflict, never a silent drop.
/// 2. **Select** — selection conditions force values; dependency-driven
///    narrowing (constraints + preconditions leaving exactly one
///    candidate) forces that candidate. Both run to a fixpoint, and
///    every forced value names the dependency responsible.
/// 3. **Check** — every constraint is evaluated against the resulting
///    configuration; a definite violation is an inconsistency with a
///    structured explanation.
pub fn evaluate(model: &ConfigModelView, input: &BTreeMap<String, ConfigValue>) -> Evaluation {
    let mut explanations: Vec<Explanation> = Vec::new();
    let mut inconsistent = false;

    // Phase 0: validate the input assignments against the domains.
    // Rejected assignments are reported and left out of the effective
    // configuration; evaluation continues so the caller sees every
    // problem at once.
    let mut effective: BTreeMap<String, ConfigValue> = BTreeMap::new();
    for (code, value) in input {
        match model.characteristic(code) {
            None => {
                inconsistent = true;
                explanations.push(Explanation {
                    dependency_id: None,
                    kind: "validation".to_string(),
                    message_parts: MessageParts {
                        characteristics: vec![code.clone()],
                        values: vec![format!("{code}={}", value.render())],
                        detail: format!("unknown characteristic '{code}' for this model"),
                    },
                });
            }
            Some(ch) => match validate_value(ch, value) {
                Ok(()) => {
                    effective.insert(code.clone(), value.clone());
                }
                Err(detail) => {
                    inconsistent = true;
                    explanations.push(Explanation {
                        dependency_id: None,
                        kind: "validation".to_string(),
                        message_parts: MessageParts {
                            characteristics: vec![code.clone()],
                            values: vec![format!("{code}={}", value.render())],
                            detail,
                        },
                    });
                }
            },
        }
    }

    let mut forced: BTreeMap<String, ForcedValue> = BTreeMap::new();
    let mut selection_conflicts: Vec<Explanation> = Vec::new();

    // Phases 1+2 to a fixpoint. Assignments are only ever added, so
    // this terminates: each pass either assigns at least one
    // characteristic or is the last.
    for _ in 0..128 {
        let mut changed = false;

        // Selection conditions forcing a characteristic value.
        for dep in model.dependencies_of_kind("selection") {
            let Some((code, want)) = selection_char_target(model, dep) else {
                continue;
            };
            if eval_expr(&dep.expr, &effective) != Some(true) {
                continue;
            }
            match effective.get(&code) {
                Some(have) if *have != want => {
                    if !selection_conflicts
                        .iter()
                        .any(|e| e.dependency_id == Some(dep.id))
                    {
                        selection_conflicts.push(explanation(
                            dep,
                            dep.expr.chars(),
                            vec![format!("{code}={}", want.render())],
                            format!(
                                "selection condition requires {code}={} but the configuration has {code}={}",
                                want.render(),
                                have.render()
                            ),
                        ));
                    }
                }
                Some(_) => {}
                None => {
                    effective.insert(code.clone(), want.clone());
                    forced.insert(
                        code,
                        ForcedValue {
                            value: want,
                            dependency_id: dep.id,
                            kind: dep.kind.clone(),
                        },
                    );
                    changed = true;
                }
            }
        }

        // Dependency-driven narrowing: a characteristic with exactly
        // one value that keeps every dependency satisfiable is forced
        // by whichever dependency eliminated the alternatives.
        for ch in &model.characteristics {
            if effective.contains_key(&ch.code) || domain_values(ch).is_empty() {
                continue;
            }
            if out_of_play_dep(model, ch, &effective).is_some() {
                continue;
            }
            let (allowed, eliminated) = allowed_values(model, ch, &effective);
            if allowed.len() == 1 && !eliminated.is_empty() {
                let dep_id = eliminated.iter().map(|(_, id)| *id).min().unwrap_or(0);
                let dep = model
                    .dependencies
                    .iter()
                    .find(|d| d.id == dep_id)
                    .expect("eliminating dependency exists in the model");
                let value = allowed.into_iter().next().unwrap();
                effective.insert(ch.code.clone(), value.clone());
                forced.insert(
                    ch.code.clone(),
                    ForcedValue {
                        value,
                        dependency_id: dep.id,
                        kind: dep.kind.clone(),
                    },
                );
                changed = true;
            }
        }

        if !changed {
            break;
        }
    }
    if !selection_conflicts.is_empty() {
        inconsistent = true;
        selection_conflicts.sort_by_key(|e| e.dependency_id.unwrap_or(0));
        explanations.extend(selection_conflicts);
    }

    // Phase 3a: chosen values whose preconditions fail are conflicts.
    for dep in model.dependencies_of_kind("precondition") {
        if eval_expr(&dep.expr, &effective) != Some(false) {
            continue;
        }
        let conflict = match parse_target(model, &dep.target) {
            Some(Target::Value(code, value)) => effective
                .get(&code)
                .map(|have| *have == value)
                .unwrap_or(false),
            Some(Target::Char(code)) => effective.contains_key(&code),
            _ => false,
        };
        if conflict {
            inconsistent = true;
            let chars = dep.expr.chars();
            explanations.push(explanation(
                dep,
                chars.clone(),
                assignment_parts(&effective, &chars),
                format!(
                    "the precondition for '{}' is not satisfied by this configuration",
                    dep.target
                ),
            ));
        }
    }

    // Phase 3b: constraints. A definite violation is an inconsistency;
    // constraints never silently repair the configuration.
    for dep in model.dependencies_of_kind("constraint") {
        if eval_expr(&dep.expr, &effective) == Some(false) {
            inconsistent = true;
            let chars = dep.expr.chars();
            explanations.push(explanation(
                dep,
                chars.clone(),
                assignment_parts(&effective, &chars),
                format!(
                    "constraint '{}' is violated by this configuration",
                    dep.target
                ),
            ));
        }
    }

    // Selectable space under the final assignments, plus the
    // completeness / dead-end analysis.
    //
    // Dead-end explanations are emitted only when the assignments so
    // far are otherwise consistent. If an earlier phase already found
    // a conflict, every remaining characteristic trivially has no
    // completable value — any candidate keeps the broken assignment —
    // and reporting each one as a dead end blames unrelated
    // dependencies for the same conflict: noise on top of the real
    // explanation (an aluminum + galvanized rejection used to repeat
    // the galvanized precondition three times, once as the conflict
    // and twice as spurious "dead ends" for mounting).
    let explain_dead_ends = !inconsistent;
    let mut selectable: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut missing: Vec<String> = Vec::new();
    for ch in &model.characteristics {
        if ch.value_type == "integer" {
            selectable.insert(ch.code.clone(), Vec::new());
            if ch.required && !effective.contains_key(&ch.code) {
                missing.push(ch.code.clone());
            }
            continue;
        }
        if out_of_play_dep(model, ch, &effective).is_some() {
            // Out of play entirely: not selectable, and exempt from
            // the required check — it cannot be chosen at all.
            selectable.insert(ch.code.clone(), Vec::new());
            continue;
        }
        let mut without = effective.clone();
        without.remove(&ch.code);
        let (allowed, eliminated) = allowed_values(model, ch, &without);
        selectable.insert(
            ch.code.clone(),
            allowed.iter().map(|v| v.render()).collect(),
        );
        if effective.contains_key(&ch.code) {
            continue;
        }
        if allowed.is_empty() {
            // Dead end: every value was eliminated by some dependency.
            inconsistent = true;
            if explain_dead_ends {
                for (value, dep_id) in &eliminated {
                    let dep = model
                        .dependencies
                        .iter()
                        .find(|d| d.id == *dep_id)
                        .expect("eliminating dependency exists in the model");
                    explanations.push(explanation(
                        dep,
                        vec![ch.code.clone()],
                        vec![format!("{}={}", ch.code, value.render())],
                        format!(
                            "no value of '{}' remains available: {}={} was eliminated by this dependency",
                            ch.code,
                            ch.code,
                            value.render()
                        ),
                    ));
                }
            }
        } else if ch.required {
            missing.push(ch.code.clone());
        }
    }

    // Component selection against the effective configuration.
    let mut selected_components: Vec<String> = Vec::new();
    let mut forced_components: Vec<ForcedComponent> = Vec::new();
    for comp in &model.components {
        let deps: Vec<&DependencyView> = model
            .dependencies_of_kind("selection")
            .into_iter()
            .filter(|dep| {
                matches!(parse_target(model, &dep.target), Some(Target::Component(code)) if code == comp.code)
            })
            .collect();
        if deps.is_empty() {
            selected_components.push(comp.code.clone());
            continue;
        }
        if let Some(dep) = deps
            .iter()
            .find(|d| eval_expr(&d.expr, &effective) == Some(true))
        {
            selected_components.push(comp.code.clone());
            forced_components.push(ForcedComponent {
                component: comp.code.clone(),
                dependency_id: dep.id,
            });
        }
    }

    let verdict = if inconsistent {
        Verdict::Inconsistent
    } else if !missing.is_empty() {
        Verdict::Incomplete { missing }
    } else {
        Verdict::Consistent
    };

    Evaluation {
        verdict,
        selectable,
        forced,
        forced_components,
        selected_components,
        explanations,
    }
}

// ---------------------------------------------------------------------------
// Persistence — load / create with save-time validation.
// ---------------------------------------------------------------------------

fn missing(what: &str, id: i64) -> impl Fn(rusqlite::Error) -> LedgerError {
    let what = what.to_string();
    move |e| match e {
        rusqlite::Error::QueryReturnedNoRows => {
            LedgerError::Invalid(format!("no {what} with id {id}"))
        }
        other => LedgerError::Db(other),
    }
}

fn load_model(conn: &Connection, model_id: i64) -> Result<ConfigModelView> {
    let (item_id, item_sku, name) = conn
        .query_row(
            "SELECT m.item_id, i.sku, m.name
             FROM config_models m JOIN items i ON i.id = m.item_id
             WHERE m.id = ?1",
            params![model_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(missing("config model", model_id))?;

    let mut ch_stmt = conn.prepare(
        "SELECT id, code, name, value_type, min_value, max_value, required
         FROM characteristics WHERE model_id = ?1 ORDER BY id",
    )?;
    let ch_rows = ch_stmt.query_map(params![model_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<i64>>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;
    let mut val_stmt = conn.prepare(
        "SELECT id, code, label FROM characteristic_values
         WHERE characteristic_id = ?1 ORDER BY id",
    )?;
    let mut characteristics = Vec::new();
    for r in ch_rows {
        let (id, code, name, value_type, min_value, max_value, required) = r?;
        let val_rows = val_stmt.query_map(params![id], |row| {
            Ok(CharacteristicValueView {
                id: row.get(0)?,
                code: row.get(1)?,
                label: row.get(2)?,
            })
        })?;
        let mut values = Vec::new();
        for v in val_rows {
            values.push(v?);
        }
        characteristics.push(CharacteristicView {
            id,
            code,
            name,
            value_type,
            min_value,
            max_value,
            required: required != 0,
            values,
        });
    }

    let mut comp_stmt = conn.prepare(
        "SELECT id, code, name, component_item_id, qty, price_delta_cents, cost_delta_cents
         FROM components WHERE model_id = ?1 ORDER BY id",
    )?;
    let comp_rows = comp_stmt.query_map(params![model_id], |row| {
        Ok(ComponentView {
            id: row.get(0)?,
            code: row.get(1)?,
            name: row.get(2)?,
            component_item_id: row.get(3)?,
            qty: row.get(4)?,
            price_delta_cents: row.get(5)?,
            cost_delta_cents: row.get(6)?,
        })
    })?;
    let mut components = Vec::new();
    for r in comp_rows {
        components.push(r?);
    }

    let mut dep_stmt = conn.prepare(
        "SELECT id, kind, target, expr, source_text, created_by
         FROM dependencies WHERE model_id = ?1 ORDER BY id",
    )?;
    let dep_rows = dep_stmt.query_map(params![model_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    let mut dependencies = Vec::new();
    for r in dep_rows {
        let (id, kind, target, expr_json, source_text, created_by) = r?;
        let expr: Expr = serde_json::from_str(&expr_json).map_err(|_| {
            LedgerError::Invalid(format!(
                "stored dependency {id} has an unreadable expression"
            ))
        })?;
        dependencies.push(DependencyView {
            id,
            model_id,
            kind,
            target,
            expr,
            source_text,
            created_by,
        });
    }

    Ok(ConfigModelView {
        id: model_id,
        item_id,
        item_sku,
        name,
        characteristics,
        components,
        dependencies,
    })
}

/// Fetch one config model with all of its parts.
pub fn get_config_model(conn: &Connection, model_id: i64) -> Result<Option<ConfigModelView>> {
    match load_model(conn, model_id) {
        Ok(m) => Ok(Some(m)),
        Err(LedgerError::Invalid(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// All config models, fully loaded, in creation order.
pub fn list_config_models(conn: &Connection) -> Result<Vec<ConfigModelView>> {
    let mut stmt = conn.prepare("SELECT id FROM config_models ORDER BY id")?;
    let ids = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for id in ids {
        out.push(load_model(conn, id?)?);
    }
    Ok(out)
}

/// The (single, active) config model for an item, if it has one.
pub fn config_model_for_item(conn: &Connection, item_id: i64) -> Result<Option<ConfigModelView>> {
    let id: Option<i64> = conn
        .query_row(
            "SELECT id FROM config_models WHERE item_id = ?1",
            params![item_id],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    match id {
        Some(id) => Ok(Some(load_model(conn, id)?)),
        None => Ok(None),
    }
}

/// Count of config models.
pub fn config_model_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM config_models", [], |r| r.get(0))?)
}

/// Create a config model for an item. An item has at most one model.
pub fn create_config_model(conn: &Connection, new: &NewConfigModel) -> Result<i64> {
    let item_known: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM items WHERE id = ?1)",
        params![new.item_id],
        |r| r.get(0),
    )?;
    if !item_known {
        return Err(LedgerError::Invalid(format!(
            "unknown item id {}",
            new.item_id
        )));
    }
    let existing: i64 = conn.query_row(
        "SELECT COUNT(*) FROM config_models WHERE item_id = ?1",
        params![new.item_id],
        |r| r.get(0),
    )?;
    if existing > 0 {
        return Err(LedgerError::Invalid(format!(
            "item {} already has a config model",
            new.item_id
        )));
    }
    conn.execute(
        "INSERT INTO config_models (item_id, name) VALUES (?1, ?2)",
        params![new.item_id, new.name],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Add a characteristic to a model. Enum characteristics need at least
/// one value; integer characteristics take min/max instead.
pub fn add_characteristic(
    conn: &Connection,
    model_id: i64,
    new: &NewCharacteristic,
) -> Result<i64> {
    load_model(conn, model_id)?; // the model must exist
    if !matches!(new.value_type.as_str(), "enum" | "integer" | "bool") {
        return Err(LedgerError::Invalid(format!(
            "value_type must be 'enum', 'integer' or 'bool', got '{}'",
            new.value_type
        )));
    }
    if new.value_type == "enum" && new.values.is_empty() {
        return Err(LedgerError::Invalid(format!(
            "enum characteristic '{}' needs at least one value",
            new.code
        )));
    }
    if let (Some(min), Some(max)) = (new.min_value, new.max_value) {
        if min > max {
            return Err(LedgerError::Invalid(format!(
                "characteristic '{}' has min {min} greater than max {max}",
                new.code
            )));
        }
    }
    let duplicate: i64 = conn.query_row(
        "SELECT COUNT(*) FROM characteristics WHERE model_id = ?1 AND code = ?2",
        params![model_id, new.code],
        |r| r.get(0),
    )?;
    if duplicate > 0 {
        return Err(LedgerError::Invalid(format!(
            "model {model_id} already has a characteristic '{}'",
            new.code
        )));
    }
    conn.execute(
        "INSERT INTO characteristics (model_id, code, name, value_type, min_value, max_value, required)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            model_id,
            new.code,
            new.name,
            new.value_type,
            new.min_value,
            new.max_value,
            new.required as i64
        ],
    )?;
    let characteristic_id = conn.last_insert_rowid();
    if new.value_type == "enum" {
        for value in &new.values {
            conn.execute(
                "INSERT INTO characteristic_values (characteristic_id, code, label)
                 VALUES (?1, ?2, ?3)",
                params![characteristic_id, value.code, value.label],
            )?;
        }
    }
    Ok(characteristic_id)
}

/// Add a component to a model.
pub fn create_component(conn: &Connection, model_id: i64, new: &NewComponent) -> Result<i64> {
    let model = load_model(conn, model_id)?;
    if new.qty <= 0 {
        return Err(LedgerError::Invalid(
            "a component quantity must be positive".to_string(),
        ));
    }
    if model.component(&new.code).is_some() {
        return Err(LedgerError::Invalid(format!(
            "model {model_id} already has a component '{}'",
            new.code
        )));
    }
    if let Some(item_id) = new.component_item_id {
        let known: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM items WHERE id = ?1)",
            params![item_id],
            |r| r.get(0),
        )?;
        if !known {
            return Err(LedgerError::Invalid(format!(
                "unknown component item id {item_id}"
            )));
        }
    }
    conn.execute(
        "INSERT INTO components (model_id, code, name, component_item_id, qty, price_delta_cents, cost_delta_cents)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            model_id,
            new.code,
            new.name,
            new.component_item_id,
            new.qty,
            new.price_delta_cents,
            new.cost_delta_cents
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

// ---------------------------------------------------------------------------
// Dependency save-time validation
// ---------------------------------------------------------------------------

fn validate_expr_value(ch: &CharacteristicView, value: &serde_json::Value) -> Result<()> {
    let ok = match ch.value_type.as_str() {
        "enum" => {
            matches!(value, serde_json::Value::String(s) if ch.values.iter().any(|v| &v.code == s))
        }
        "integer" => value.as_i64().is_some(),
        "bool" => value.is_boolean(),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(LedgerError::Invalid(format!(
            "value {value} does not fit characteristic '{}' (type {})",
            ch.code, ch.value_type
        )))
    }
}

fn validate_expr(model: &ConfigModelView, expr: &Expr) -> Result<()> {
    match expr {
        Expr::Eq { char_code, value } => {
            let ch = model.characteristic(char_code).ok_or_else(|| {
                LedgerError::Invalid(format!(
                    "unknown characteristic '{char_code}' in expression"
                ))
            })?;
            validate_expr_value(ch, value)
        }
        Expr::In { char_code, values } => {
            let ch = model.characteristic(char_code).ok_or_else(|| {
                LedgerError::Invalid(format!(
                    "unknown characteristic '{char_code}' in expression"
                ))
            })?;
            for value in values {
                validate_expr_value(ch, value)?;
            }
            Ok(())
        }
        Expr::Cmp { char_code, .. } => {
            let ch = model.characteristic(char_code).ok_or_else(|| {
                LedgerError::Invalid(format!(
                    "unknown characteristic '{char_code}' in expression"
                ))
            })?;
            if ch.value_type != "integer" {
                return Err(LedgerError::Invalid(format!(
                    "comparison needs an integer characteristic, but '{char_code}' is {}",
                    ch.value_type
                )));
            }
            Ok(())
        }
        Expr::And { all } => {
            for e in all {
                validate_expr(model, e)?;
            }
            Ok(())
        }
        Expr::Or { any } => {
            for e in any {
                validate_expr(model, e)?;
            }
            Ok(())
        }
        Expr::Not { expr } => validate_expr(model, expr),
    }
}

/// The characteristic a dependency *drives*, if any: preconditions and
/// value selections drive their target characteristic; component
/// selections and constraints drive none.
fn driven_char(model: &ConfigModelView, kind: &str, target: &str) -> Option<String> {
    if kind != "precondition" && kind != "selection" {
        return None;
    }
    match parse_target(model, target) {
        Some(Target::Char(code)) => Some(code),
        Some(Target::Value(code, _)) => Some(code),
        _ => None,
    }
}

/// Reject a dependency that would close a cycle in the assignment
/// graph. Edges run from every characteristic a dependency *reads*
/// (its expression) to the characteristic it *drives* (its target).
/// Constraints drive nothing — they are checked, never assigned — so
/// they cannot loop the evaluator and stay out of the graph.
fn assert_no_cycle(model: &ConfigModelView, new: &NewDependency) -> Result<()> {
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut push_edges = |kind: &str, target: &str, expr: &Expr| {
        if let Some(driven) = driven_char(model, kind, target) {
            for read in expr.chars() {
                edges.push((read, driven.clone()));
            }
        }
    };
    for dep in &model.dependencies {
        push_edges(&dep.kind, &dep.target, &dep.expr);
    }
    push_edges(&new.kind, &new.target, &new.expr);

    // Depth-first cycle check over characteristic codes.
    let nodes: Vec<String> = model
        .characteristics
        .iter()
        .map(|c| c.code.clone())
        .collect();
    fn visit(
        node: &str,
        edges: &[(String, String)],
        state: &mut Vec<(String, u8)>, // 1 = on stack, 2 = done
        stack: &mut Vec<String>,
    ) -> Option<Vec<String>> {
        state.push((node.to_string(), 1));
        stack.push(node.to_string());
        for (_, to) in edges.iter().filter(|(f, _)| f == node) {
            if let Some((_, s)) = state.iter().find(|(n, _)| n == to) {
                if *s == 1 {
                    let start = stack.iter().position(|n| n == to).unwrap_or(0);
                    let mut cycle = stack[start..].to_vec();
                    cycle.push(to.clone());
                    return Some(cycle);
                }
                continue;
            }
            if let Some(cycle) = visit(to, edges, state, stack) {
                return Some(cycle);
            }
        }
        stack.pop();
        state.retain(|(n, _)| n != node);
        state.push((node.to_string(), 2));
        None
    }
    let mut state: Vec<(String, u8)> = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    for node in &nodes {
        if state.iter().any(|(n, _)| n == node) {
            continue;
        }
        if let Some(cycle) = visit(node, &edges, &mut state, &mut stack) {
            return Err(LedgerError::Invalid(format!(
                "dependency would create a cycle: {}",
                cycle.join(" -> ")
            )));
        }
    }
    Ok(())
}

/// Save a dependency on a model, after validating its kind, target and
/// expression against the model and rejecting dependency cycles. The
/// expression is stored as typed JSON — it is never parsed from text,
/// here or anywhere else.
pub fn create_dependency(conn: &Connection, model_id: i64, new: &NewDependency) -> Result<i64> {
    let model = load_model(conn, model_id)?;
    if !matches!(
        new.kind.as_str(),
        "precondition" | "selection" | "constraint"
    ) {
        return Err(LedgerError::Invalid(format!(
            "dependency kind must be 'precondition', 'selection' or 'constraint', got '{}'",
            new.kind
        )));
    }
    if new.target.is_empty() {
        return Err(LedgerError::Invalid(
            "a dependency needs a target".to_string(),
        ));
    }
    match new.kind.as_str() {
        "precondition" => match parse_target(&model, &new.target) {
            Some(Target::Char(code)) | Some(Target::Value(code, _)) => {
                if model.characteristic(&code).is_none() {
                    return Err(LedgerError::Invalid(format!(
                        "precondition target names unknown characteristic '{code}'"
                    )));
                }
            }
            _ => {
                return Err(LedgerError::Invalid(format!(
                    "precondition target must be a characteristic or value, got '{}'",
                    new.target
                )))
            }
        },
        "selection" => match parse_target(&model, &new.target) {
            Some(Target::Component(code)) => {
                if model.component(&code).is_none() {
                    return Err(LedgerError::Invalid(format!(
                        "selection target names unknown component '{code}'"
                    )));
                }
            }
            Some(Target::Value(code, _)) => {
                if model.characteristic(&code).is_none() {
                    return Err(LedgerError::Invalid(format!(
                        "selection target names unknown characteristic '{code}'"
                    )));
                }
            }
            _ => {
                return Err(LedgerError::Invalid(format!(
                    "selection target must be a value (char=value) or a component (component:code), got '{}'",
                    new.target
                )))
            }
        },
        _ => {} // constraint targets are free labels
    }
    validate_expr(&model, &new.expr)?;
    assert_no_cycle(&model, new)?;
    let expr_json = serde_json::to_string(&new.expr).expect("expression serializes");
    conn.execute(
        "INSERT INTO dependencies (model_id, kind, target, expr, source_text, created_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            model_id,
            new.kind,
            new.target,
            expr_json,
            new.source_text,
            new.created_by
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Evaluate a configuration against a stored model: load, then run the
/// pure evaluator. Convenience wrapper — evaluation itself writes
/// nothing.
pub fn evaluate_configuration(
    conn: &Connection,
    model_id: i64,
    config: &BTreeMap<String, ConfigValue>,
) -> Result<Evaluation> {
    let model = load_model(conn, model_id)?;
    Ok(evaluate(&model, config))
}

/// A stable fingerprint of a model's dependency set (ids, kinds,
/// targets, expression trees). Stored in quote-line snapshots so a
/// later dependency edit is visible as a fingerprint change — and can
/// never rewrite the snapshot itself.
pub fn dependency_fingerprint(model: &ConfigModelView) -> String {
    // FNV-1a over the canonical form of every dependency, in id order.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for dep in &model.dependencies {
        let canonical = format!(
            "{}|{}|{}|{}",
            dep.id,
            dep.kind,
            dep.target,
            serde_json::to_string(&dep.expr).unwrap_or_default()
        );
        for b in canonical.bytes() {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

// ---------------------------------------------------------------------------
// Demo seed — the BRK-100 variant model from the design doc's sketch.
// ---------------------------------------------------------------------------

/// Price delta of the aluminum blank over the base bracket: +$18.00.
pub const ALUMINUM_BLANK_PRICE_DELTA_CENTS: Cents = 1_800;
/// Cost delta of the aluminum blank: +$11.00.
pub const ALUMINUM_BLANK_COST_DELTA_CENTS: Cents = 1_100;
/// Price delta of the powder-coat finish treatment: +$12.00.
pub const POWDER_COAT_PRICE_DELTA_CENTS: Cents = 1_200;
/// Cost delta of the powder-coat finish treatment: +$7.00.
pub const POWDER_COAT_COST_DELTA_CENTS: Cents = 700;
/// Price delta of the galvanized finish treatment: +$15.00.
pub const GALVANIZED_PRICE_DELTA_CENTS: Cents = 1_500;
/// Cost delta of the galvanized finish treatment: +$9.00.
pub const GALVANIZED_COST_DELTA_CENTS: Cents = 900;
/// Price delta of the reinforced mount kit: +$22.00.
pub const REINFORCED_MOUNT_PRICE_DELTA_CENTS: Cents = 2_200;
/// Cost delta of the reinforced mount kit: +$14.00.
pub const REINFORCED_MOUNT_COST_DELTA_CENTS: Cents = 1_400;

/// Seed the BRK-100 demo variant model if no config model exists yet:
/// material / finish / mounting / quantity characteristics, the
/// component list with its price/cost deltas, and the design doc's
/// three illustrative dependencies (galvanized-needs-steel
/// precondition, blank + finish + mount selections, and the
/// over-40-units-need-reinforced constraint). Returns true if seeding
/// happened. Writes no journal entries and no documents.
pub fn seed_configurator_if_empty(conn: &Connection) -> Result<bool> {
    if config_model_count(conn)? > 0 {
        return Ok(false);
    }
    let item_id: i64 = conn
        .query_row(
            "SELECT id FROM items WHERE sku = ?1",
            params![crate::BRACKET_SKU],
            |r| r.get(0),
        )
        .map_err(|_| LedgerError::Invalid("bracket item is not seeded".to_string()))?;

    let model_id = create_config_model(
        conn,
        &NewConfigModel {
            item_id,
            name: "BRK-100 variant model".to_string(),
        },
    )?;

    let enum_char = |code: &str, name: &str, values: Vec<(&str, &str)>| NewCharacteristic {
        code: code.to_string(),
        name: name.to_string(),
        value_type: "enum".to_string(),
        min_value: None,
        max_value: None,
        required: true,
        values: values
            .into_iter()
            .map(|(code, label)| NewCharacteristicValue {
                code: code.to_string(),
                label: label.to_string(),
            })
            .collect(),
    };
    add_characteristic(
        conn,
        model_id,
        &enum_char(
            "material",
            "Bracket material",
            vec![("steel", "Steel"), ("aluminum", "Aluminum")],
        ),
    )?;
    add_characteristic(
        conn,
        model_id,
        &enum_char(
            "finish",
            "Finish",
            vec![
                ("bare", "Bare"),
                ("powder-coat", "Powder coat"),
                ("galvanized", "Galvanized"),
            ],
        ),
    )?;
    add_characteristic(
        conn,
        model_id,
        &enum_char(
            "mounting",
            "Mounting",
            vec![
                ("standard", "Standard mount"),
                ("reinforced", "Reinforced mount"),
            ],
        ),
    )?;
    add_characteristic(
        conn,
        model_id,
        &NewCharacteristic {
            code: "quantity".to_string(),
            name: "Order quantity".to_string(),
            value_type: "integer".to_string(),
            min_value: Some(1),
            max_value: None,
            required: true,
            values: Vec::new(),
        },
    )?;

    let component = |code: &str, name: &str, price: Cents, cost: Cents| NewComponent {
        code: code.to_string(),
        name: name.to_string(),
        component_item_id: None,
        qty: 1,
        price_delta_cents: price,
        cost_delta_cents: cost,
    };
    // The steel blank is the baseline material: included in the base
    // price, so its deltas are zero. The aluminum blank is the upgrade.
    create_component(
        conn,
        model_id,
        &component("steel-blank", "Steel blank", 0, 0),
    )?;
    create_component(
        conn,
        model_id,
        &component(
            "aluminum-blank",
            "Aluminum blank",
            ALUMINUM_BLANK_PRICE_DELTA_CENTS,
            ALUMINUM_BLANK_COST_DELTA_CENTS,
        ),
    )?;
    create_component(
        conn,
        model_id,
        &component(
            "powder-coat-finish",
            "Powder-coat finish treatment",
            POWDER_COAT_PRICE_DELTA_CENTS,
            POWDER_COAT_COST_DELTA_CENTS,
        ),
    )?;
    create_component(
        conn,
        model_id,
        &component(
            "galvanized-finish",
            "Galvanized finish treatment",
            GALVANIZED_PRICE_DELTA_CENTS,
            GALVANIZED_COST_DELTA_CENTS,
        ),
    )?;
    create_component(
        conn,
        model_id,
        &component(
            "reinforced-mount",
            "Reinforced mount kit",
            REINFORCED_MOUNT_PRICE_DELTA_CENTS,
            REINFORCED_MOUNT_COST_DELTA_CENTS,
        ),
    )?;

    let dep = |kind: &str, target: &str, expr: Expr, source_text: &str| NewDependency {
        kind: kind.to_string(),
        target: target.to_string(),
        expr,
        source_text: source_text.to_string(),
        created_by: "seed".to_string(),
    };
    let eq = |char_code: &str, value: &str| Expr::Eq {
        char_code: char_code.to_string(),
        value: serde_json::Value::String(value.to_string()),
    };

    // Precondition: galvanized finish is in play only for steel.
    create_dependency(
        conn,
        model_id,
        &dep(
            "precondition",
            "finish=galvanized",
            eq("material", "steel"),
            "Galvanized finish is available only on steel brackets.",
        ),
    )?;
    // Selections: the variant decides which components are in the list.
    create_dependency(
        conn,
        model_id,
        &dep(
            "selection",
            "component:steel-blank",
            eq("material", "steel"),
            "A steel bracket is built from the steel blank.",
        ),
    )?;
    create_dependency(
        conn,
        model_id,
        &dep(
            "selection",
            "component:aluminum-blank",
            eq("material", "aluminum"),
            "An aluminum bracket is built from the aluminum blank instead of the steel blank.",
        ),
    )?;
    create_dependency(
        conn,
        model_id,
        &dep(
            "selection",
            "component:powder-coat-finish",
            eq("finish", "powder-coat"),
            "A powder-coat finish adds the powder-coating treatment.",
        ),
    )?;
    create_dependency(
        conn,
        model_id,
        &dep(
            "selection",
            "component:galvanized-finish",
            eq("finish", "galvanized"),
            "A galvanized finish adds the galvanizing treatment.",
        ),
    )?;
    create_dependency(
        conn,
        model_id,
        &dep(
            "selection",
            "component:reinforced-mount",
            eq("mounting", "reinforced"),
            "Reinforced mounting adds the reinforced mount kit.",
        ),
    )?;
    // Constraint: quantity > 40 requires reinforced mounting, written
    // as the implication Or(Not(quantity > 40), mounting = reinforced).
    create_dependency(
        conn,
        model_id,
        &dep(
            "constraint",
            "quantity-requires-reinforced-mounting",
            Expr::Or {
                any: vec![
                    Expr::Not {
                        expr: Box::new(Expr::Cmp {
                            char_code: "quantity".to_string(),
                            op: CmpOp::Gt,
                            value: 40,
                        }),
                    },
                    eq("mounting", "reinforced"),
                ],
            },
            "Orders over 40 units need the reinforced mount.",
        ),
    )?;

    Ok(true)
}
