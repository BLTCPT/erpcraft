//! ERPCraft HTTP server — a thin Axum shell over `erpcraft-core`.
//!
//! Day 1 surface: health/root, chart of accounts, trial balance, and journal
//! posting/read-back. Day 2 adds the order-to-cash documents (customers,
//! items, quotes, sales orders, invoices, payments). Day 3 adds the GL
//! posting engine behind those transitions (in the core) plus the
//! account ledger view (`GET /accounts/{code}/entries`). Day 4 closes
//! the returns gap: `POST /invoices/{id}/returns` records a sales
//! return (the core function shipped on Day 3 without a route). Every
//! write goes through the ledger core; invariant violations come back
//! as 400 with the core's plain-language message.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use erpcraft_core::rusqlite::Connection;
use erpcraft_core::{
    accept_quote, account_count, account_ledger, add_characteristic, approve_requisition,
    cancel_order, cancel_purchase_order, cancel_requisition, close_fiscal_period,
    config_model_count, create_component, create_config_model, create_dependency,
    create_fiscal_period, create_invoice_from_order, create_order_from_quote,
    create_purchase_order, create_quote, create_requisition, create_supplier, create_user,
    customer_count, decline_quote, entry_count, evaluate, expire_quote, fulfil_order,
    get_ap_invoice, get_config_model, get_entry, get_invoice, get_order, get_procurement_order,
    get_quote, get_requisition, invoice_count, issue_invoice, list_accounts, list_ap_invoices,
    list_ap_payments, list_ap_payments_for_invoice, list_config_models, list_control_settings,
    list_customers, list_entries, list_fiscal_periods, list_goods_receipts, list_invoices,
    list_items, list_orders, list_payments, list_payments_for_invoice, list_procurement_orders,
    list_quotes, list_requisitions, list_suppliers, list_users, open_db, order_count,
    override_ap_match, pay_ap_invoice_attributed, payment_count, post_entry, quote_count,
    receive_goods, record_ap_invoice, record_payment_attributed, record_sales_return,
    reject_requisition, send_quote, set_approval_threshold, trial_balance, void_invoice,
    ApLineInput, AuditFilter, ConfigValue, Explanation, LedgerError, NewApInvoice,
    NewCharacteristic, NewComponent, NewConfigModel, NewDependency, NewEntry, NewProcurementOrder,
    NewQuote, NewRequisition, ReceiveLineInput, ReturnLine, DEMO_COMPANY,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

type SharedDb = Arc<Mutex<Connection>>;

#[derive(Serialize)]
struct RootInfo {
    name: &'static str,
    vision: &'static str,
    status: &'static str,
    demo_company: &'static str,
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    accounts: i64,
    journal_entries: i64,
    customers: i64,
    quotes: i64,
    orders: i64,
    invoices: i64,
    payments: i64,
    config_models: i64,
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    /// Structured configurator explanations, when the refusal came
    /// from the evaluator. Omitted for plain errors.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    explanations: Vec<Explanation>,
}

impl ErrorBody {
    fn plain(error: String) -> Self {
        ErrorBody {
            error,
            explanations: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
struct CreateOrder {
    quote_id: i64,
}

#[derive(Deserialize)]
struct CreateInvoice {
    order_id: i64,
}

#[derive(Deserialize)]
struct RecordPayment {
    amount_cents: i64,
    /// Controls approval rule (optional): who is paying, and the
    /// second approver when the amount is above the threshold.
    actor: Option<String>,
    approver: Option<String>,
}

#[derive(Deserialize)]
struct RecordReturn {
    lines: Vec<ReturnLine>,
}

#[derive(Deserialize)]
struct CreateSupplier {
    name: String,
}

#[derive(Deserialize)]
struct ReceiveGoods {
    lines: Vec<ReceiveLineInput>,
}

#[derive(Deserialize)]
struct RecordApInvoice {
    lines: Vec<ApLineInput>,
    /// The supplier's own invoice number — the duplicate key.
    #[serde(default)]
    supplier_ref: Option<String>,
}

#[derive(Deserialize)]
struct OverrideMatch {
    reason: String,
    by: String,
}

#[derive(Deserialize)]
struct CreateFiscalPeriod {
    name: String,
    start_date: String,
    end_date: String,
}

#[derive(Deserialize)]
struct CreateUser {
    name: String,
    role: String,
}

#[derive(Deserialize)]
struct SetApprovalThreshold {
    threshold_cents: i64,
}

#[derive(Deserialize)]
struct EvaluateRequest {
    configuration: BTreeMap<String, ConfigValue>,
}

/// A refused state transition / validation failure is the client's error:
/// 400 with the core's plain-language message. Configurator refusals
/// also carry the evaluator's structured explanations.
fn bad_request(e: LedgerError) -> (StatusCode, Json<ErrorBody>) {
    match e {
        LedgerError::Configuration {
            message,
            explanations,
        } => (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: message,
                explanations,
            }),
        ),
        other => (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody::plain(other.to_string())),
        ),
    }
}

fn not_found(what: &str, id: i64) -> (StatusCode, Json<ErrorBody>) {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorBody::plain(format!("no {what} with id {id}"))),
    )
}

fn server_error(e: LedgerError) -> (StatusCode, Json<ErrorBody>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorBody::plain(e.to_string())),
    )
}

async fn root() -> Json<RootInfo> {
    Json(RootInfo {
        name: "ERPCraft",
        vision: "A free, open-source, clean-room ERP in Rust — AI-native from day one.",
        status: "alpha: ledger core + order-to-cash with GL posting engine + variant configurator (typed dependencies, deterministic evaluation, roll-up pricing) + audit trail + dynamic MRP (Warp Speed pattern)",
        demo_company: DEMO_COMPANY,
    })
}

async fn health(State(db): State<SharedDb>) -> Json<Health> {
    let conn = db.lock().expect("db lock");
    Json(Health {
        status: "ok",
        accounts: account_count(&conn).unwrap_or(-1),
        journal_entries: entry_count(&conn).unwrap_or(-1),
        customers: customer_count(&conn).unwrap_or(-1),
        quotes: quote_count(&conn).unwrap_or(-1),
        orders: order_count(&conn).unwrap_or(-1),
        invoices: invoice_count(&conn).unwrap_or(-1),
        payments: payment_count(&conn).unwrap_or(-1),
        config_models: config_model_count(&conn).unwrap_or(-1),
    })
}

async fn accounts(State(db): State<SharedDb>) -> Result<Json<serde_json::Value>, StatusCode> {
    let conn = db.lock().expect("db lock");
    let accounts = list_accounts(&conn).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "accounts": accounts })))
}

async fn trial_balance_handler(
    State(db): State<SharedDb>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let conn = db.lock().expect("db lock");
    let tb = trial_balance(&conn).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::to_value(tb).expect("serializable")))
}

/// Day 3: the account ledger — one account's posted lines in entry order
/// with a running balance. An unknown account code is a 404.
async fn account_entries_handler(
    State(db): State<SharedDb>,
    Path(code): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    match account_ledger(&conn, &code) {
        Ok(ledger) => Ok(Json(serde_json::to_value(ledger).expect("serializable"))),
        Err(LedgerError::UnknownAccount(_)) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorBody::plain(format!("no account with code {code}"))),
        )),
        Err(e) => Err(server_error(e)),
    }
}

async fn list_entries_handler(
    State(db): State<SharedDb>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let conn = db.lock().expect("db lock");
    let entries = list_entries(&conn).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "journal_entries": entries })))
}

/// Day 6: the audit trail — every posted entry fully explained,
/// with the same optional filters as the `audit_trail` dispatch
/// command (source / document / from_date / to_date).
#[derive(Deserialize, Default)]
struct AuditQuery {
    source: Option<String>,
    document: Option<String>,
    from_date: Option<String>,
    to_date: Option<String>,
}

async fn audit_trail_handler(
    State(db): State<SharedDb>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let filter = AuditFilter {
        source: q.source,
        document: q.document,
        from_date: q.from_date,
        to_date: q.to_date,
    };
    match erpcraft_core::audit_trail(&conn, &filter) {
        Ok(records) => Ok(Json(serde_json::json!({ "audit_trail": records }))),
        Err(e) => Err(bad_request(e)),
    }
}

/// Day 6: one entry's full provenance — its document chain and
/// reversal links. 404 when the entry does not exist.
async fn audit_entry_handler(
    State(db): State<SharedDb>,
    Path(entry_id): Path<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    match erpcraft_core::audit_entry(&conn, entry_id) {
        Ok(Some(record)) => Ok(Json(serde_json::to_value(record).expect("serializable"))),
        Ok(None) => Err(not_found("journal entry", entry_id)),
        Err(e) => Err(server_error(e)),
    }
}

async fn post_entry_handler(
    State(db): State<SharedDb>,
    Json(new): Json<NewEntry>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let mut conn = db.lock().expect("db lock");
    match post_entry(&mut conn, &new) {
        Ok(id) => match get_entry(&conn, id) {
            Ok(Some(entry)) => Ok((
                StatusCode::CREATED,
                Json(serde_json::to_value(entry).expect("serializable")),
            )),
            _ => Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody::plain(
                    "posted but could not be read back".to_string(),
                )),
            )),
        },
        // Invariant violations are the client's error: the database refused.
        Err(e) => Err(bad_request(e)),
    }
}

// ---------------------------------------------------------------------------
// Day 2: master data
// ---------------------------------------------------------------------------

async fn customers(State(db): State<SharedDb>) -> Result<Json<serde_json::Value>, StatusCode> {
    let conn = db.lock().expect("db lock");
    let customers = list_customers(&conn).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "customers": customers })))
}

async fn items(State(db): State<SharedDb>) -> Result<Json<serde_json::Value>, StatusCode> {
    let conn = db.lock().expect("db lock");
    let items = list_items(&conn).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "items": items })))
}

// ---------------------------------------------------------------------------
// Day 2: quotes
// ---------------------------------------------------------------------------

async fn list_quotes_handler(
    State(db): State<SharedDb>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let conn = db.lock().expect("db lock");
    let quotes = list_quotes(&conn).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "quotes": quotes })))
}

async fn create_quote_handler(
    State(db): State<SharedDb>,
    Json(new): Json<NewQuote>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let mut conn = db.lock().expect("db lock");
    let id = create_quote(&mut conn, &new).map_err(bad_request)?;
    let quote = get_quote(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("quote", id))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(quote).expect("serializable")),
    ))
}

async fn get_quote_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let quote = get_quote(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("quote", id))?;
    Ok(Json(serde_json::to_value(quote).expect("serializable")))
}

/// Shared shape of the quote transition handlers: apply the transition,
/// then read the quote back.
macro_rules! quote_transition {
    ($name:ident, $core_fn:ident) => {
        async fn $name(
            State(db): State<SharedDb>,
            Path(id): Path<i64>,
        ) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
            let mut conn = db.lock().expect("db lock");
            $core_fn(&mut conn, id).map_err(bad_request)?;
            let quote = get_quote(&conn, id)
                .map_err(server_error)?
                .ok_or_else(|| not_found("quote", id))?;
            Ok(Json(serde_json::to_value(quote).expect("serializable")))
        }
    };
}

quote_transition!(send_quote_handler, send_quote);
quote_transition!(accept_quote_handler, accept_quote);
quote_transition!(decline_quote_handler, decline_quote);
quote_transition!(expire_quote_handler, expire_quote);

// ---------------------------------------------------------------------------
// Day 2: sales orders
// ---------------------------------------------------------------------------

async fn list_orders_handler(
    State(db): State<SharedDb>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let conn = db.lock().expect("db lock");
    let orders = list_orders(&conn).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "orders": orders })))
}

async fn create_order_handler(
    State(db): State<SharedDb>,
    Json(body): Json<CreateOrder>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let mut conn = db.lock().expect("db lock");
    let id = create_order_from_quote(&mut conn, body.quote_id).map_err(bad_request)?;
    let order = get_order(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("sales order", id))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(order).expect("serializable")),
    ))
}

async fn get_order_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let order = get_order(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("sales order", id))?;
    Ok(Json(serde_json::to_value(order).expect("serializable")))
}

macro_rules! order_transition {
    ($name:ident, $core_fn:ident) => {
        async fn $name(
            State(db): State<SharedDb>,
            Path(id): Path<i64>,
        ) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
            let mut conn = db.lock().expect("db lock");
            $core_fn(&mut conn, id).map_err(bad_request)?;
            let order = get_order(&conn, id)
                .map_err(server_error)?
                .ok_or_else(|| not_found("sales order", id))?;
            Ok(Json(serde_json::to_value(order).expect("serializable")))
        }
    };
}

order_transition!(fulfil_order_handler, fulfil_order);
order_transition!(cancel_order_handler, cancel_order);

// ---------------------------------------------------------------------------
// Day 2: invoices and payments
// ---------------------------------------------------------------------------

async fn list_invoices_handler(
    State(db): State<SharedDb>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let conn = db.lock().expect("db lock");
    let invoices = list_invoices(&conn).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "invoices": invoices })))
}

async fn create_invoice_handler(
    State(db): State<SharedDb>,
    Json(body): Json<CreateInvoice>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let mut conn = db.lock().expect("db lock");
    let id = create_invoice_from_order(&mut conn, body.order_id).map_err(bad_request)?;
    let invoice = get_invoice(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("invoice", id))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(invoice).expect("serializable")),
    ))
}

async fn get_invoice_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let invoice = get_invoice(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("invoice", id))?;
    Ok(Json(serde_json::to_value(invoice).expect("serializable")))
}

macro_rules! invoice_transition {
    ($name:ident, $core_fn:ident) => {
        async fn $name(
            State(db): State<SharedDb>,
            Path(id): Path<i64>,
        ) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
            let mut conn = db.lock().expect("db lock");
            $core_fn(&mut conn, id).map_err(bad_request)?;
            let invoice = get_invoice(&conn, id)
                .map_err(server_error)?
                .ok_or_else(|| not_found("invoice", id))?;
            Ok(Json(serde_json::to_value(invoice).expect("serializable")))
        }
    };
}

invoice_transition!(issue_invoice_handler, issue_invoice);
invoice_transition!(void_invoice_handler, void_invoice);

async fn record_payment_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
    Json(body): Json<RecordPayment>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let mut conn = db.lock().expect("db lock");
    let payment_id = record_payment_attributed(
        &mut conn,
        id,
        body.amount_cents,
        body.actor.as_deref(),
        body.approver.as_deref(),
    )
    .map_err(bad_request)?;
    let payments = list_payments_for_invoice(&conn, id).map_err(server_error)?;
    let payment = payments
        .into_iter()
        .find(|p| p.id == payment_id)
        .ok_or_else(|| not_found("payment", payment_id))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(payment).expect("serializable")),
    ))
}

/// Day 4: record a sales return against an issued invoice. The core
/// posts two reversing entries (revenue + restock), each pointing at
/// the invoice entry it corrects; both are read back in the response,
/// the same read-back rule as every other posting on this surface.
async fn record_return_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
    Json(body): Json<RecordReturn>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let mut conn = db.lock().expect("db lock");
    let (revenue_entry_id, cogs_entry_id) =
        record_sales_return(&mut conn, id, &body.lines).map_err(bad_request)?;
    let revenue_entry = get_entry(&conn, revenue_entry_id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("journal entry", revenue_entry_id))?;
    let cogs_entry = get_entry(&conn, cogs_entry_id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("journal entry", cogs_entry_id))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "revenue_entry": revenue_entry,
            "cogs_entry": cogs_entry,
        })),
    ))
}

async fn list_payments_handler(
    State(db): State<SharedDb>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let conn = db.lock().expect("db lock");
    let payments = list_payments(&conn).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "payments": payments })))
}

async fn list_invoice_payments_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    // 404 rather than an empty list for an invoice that does not exist.
    get_invoice(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("invoice", id))?;
    let payments = list_payments_for_invoice(&conn, id).map_err(server_error)?;
    Ok(Json(serde_json::json!({ "payments": payments })))
}

// ---------------------------------------------------------------------------
// Variant configurator
// ---------------------------------------------------------------------------

async fn list_config_models_handler(
    State(db): State<SharedDb>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let conn = db.lock().expect("db lock");
    let models = list_config_models(&conn).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "config_models": models })))
}

async fn create_config_model_handler(
    State(db): State<SharedDb>,
    Json(new): Json<NewConfigModel>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let id = create_config_model(&conn, &new).map_err(bad_request)?;
    let model = get_config_model(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("config model", id))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(model).expect("serializable")),
    ))
}

async fn get_config_model_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let model = get_config_model(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("config model", id))?;
    Ok(Json(serde_json::to_value(model).expect("serializable")))
}

async fn add_characteristic_handler(
    State(db): State<SharedDb>,
    Path(model_id): Path<i64>,
    Json(new): Json<NewCharacteristic>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let id = add_characteristic(&conn, model_id, &new).map_err(bad_request)?;
    let model = get_config_model(&conn, model_id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("config model", model_id))?;
    let characteristic = model
        .characteristics
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| not_found("characteristic", id))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(characteristic).expect("serializable")),
    ))
}

async fn create_component_handler(
    State(db): State<SharedDb>,
    Path(model_id): Path<i64>,
    Json(new): Json<NewComponent>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let id = create_component(&conn, model_id, &new).map_err(bad_request)?;
    let model = get_config_model(&conn, model_id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("config model", model_id))?;
    let component = model
        .components
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| not_found("component", id))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(component).expect("serializable")),
    ))
}

async fn create_dependency_handler(
    State(db): State<SharedDb>,
    Path(model_id): Path<i64>,
    Json(new): Json<NewDependency>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let id = create_dependency(&conn, model_id, &new).map_err(bad_request)?;
    let model = get_config_model(&conn, model_id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("config model", model_id))?;
    let dependency = model
        .dependencies
        .into_iter()
        .find(|d| d.id == id)
        .ok_or_else(|| not_found("dependency", id))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(dependency).expect("serializable")),
    ))
}

/// Evaluation is a pure read: even an inconsistent configuration is a
/// successful evaluation — the verdict and its structured explanations
/// are the response body.
async fn evaluate_handler(
    State(db): State<SharedDb>,
    Path(model_id): Path<i64>,
    Json(body): Json<EvaluateRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let model = get_config_model(&conn, model_id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("config model", model_id))?;
    let evaluation = evaluate(&model, &body.configuration);
    Ok(Json(
        serde_json::to_value(evaluation).expect("serializable"),
    ))
}

// ---------------------------------------------------------------------------
// Procure-to-pay (ENTERPRISE_GAPS.md §3): suppliers, requisitions,
// procurement orders, goods receipts, AP invoices, AP payments.
// ---------------------------------------------------------------------------

async fn list_suppliers_handler(
    State(db): State<SharedDb>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let suppliers = list_suppliers(&conn).map_err(server_error)?;
    Ok(Json(serde_json::to_value(suppliers).expect("serializable")))
}

async fn create_supplier_handler(
    State(db): State<SharedDb>,
    Json(body): Json<CreateSupplier>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let id = create_supplier(&conn, &body.name).map_err(bad_request)?;
    let supplier = list_suppliers(&conn)
        .map_err(server_error)?
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| not_found("supplier", id))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(supplier).expect("serializable")),
    ))
}

async fn list_requisitions_handler(
    State(db): State<SharedDb>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let reqs = list_requisitions(&conn).map_err(server_error)?;
    Ok(Json(serde_json::to_value(reqs).expect("serializable")))
}

async fn create_requisition_handler(
    State(db): State<SharedDb>,
    Json(new): Json<NewRequisition>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let id = create_requisition(&conn, &new).map_err(bad_request)?;
    let req = get_requisition(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("purchase requisition", id))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(req).expect("serializable")),
    ))
}

async fn get_requisition_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let req = get_requisition(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("purchase requisition", id))?;
    Ok(Json(serde_json::to_value(req).expect("serializable")))
}

async fn approve_requisition_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    approve_requisition(&conn, id).map_err(bad_request)?;
    let req = get_requisition(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("purchase requisition", id))?;
    Ok(Json(serde_json::to_value(req).expect("serializable")))
}

async fn reject_requisition_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    reject_requisition(&conn, id).map_err(bad_request)?;
    let req = get_requisition(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("purchase requisition", id))?;
    Ok(Json(serde_json::to_value(req).expect("serializable")))
}

async fn cancel_requisition_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    cancel_requisition(&conn, id).map_err(bad_request)?;
    let req = get_requisition(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("purchase requisition", id))?;
    Ok(Json(serde_json::to_value(req).expect("serializable")))
}

async fn list_procurement_orders_handler(
    State(db): State<SharedDb>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let orders = list_procurement_orders(&conn).map_err(server_error)?;
    Ok(Json(serde_json::to_value(orders).expect("serializable")))
}

async fn create_purchase_order_handler(
    State(db): State<SharedDb>,
    Json(new): Json<NewProcurementOrder>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let mut conn = db.lock().expect("db lock");
    let id = create_purchase_order(&mut conn, &new).map_err(bad_request)?;
    let order = get_procurement_order(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("procurement order", id))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(order).expect("serializable")),
    ))
}

async fn get_procurement_order_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let order = get_procurement_order(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("procurement order", id))?;
    Ok(Json(serde_json::to_value(order).expect("serializable")))
}

async fn cancel_purchase_order_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    cancel_purchase_order(&conn, id).map_err(bad_request)?;
    let order = get_procurement_order(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("procurement order", id))?;
    Ok(Json(serde_json::to_value(order).expect("serializable")))
}

async fn receive_goods_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
    Json(body): Json<ReceiveGoods>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let mut conn = db.lock().expect("db lock");
    let receipt_id = receive_goods(&mut conn, id, &body.lines).map_err(bad_request)?;
    let receipt = list_goods_receipts(&conn, Some(id))
        .map_err(server_error)?
        .into_iter()
        .find(|r| r.id == receipt_id)
        .ok_or_else(|| not_found("goods receipt", receipt_id))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(receipt).expect("serializable")),
    ))
}

async fn list_goods_receipts_handler(
    State(db): State<SharedDb>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let receipts = list_goods_receipts(&conn, None).map_err(server_error)?;
    Ok(Json(serde_json::to_value(receipts).expect("serializable")))
}

async fn record_ap_invoice_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
    Json(body): Json<RecordApInvoice>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let mut conn = db.lock().expect("db lock");
    let ap_id = record_ap_invoice(
        &mut conn,
        &NewApInvoice {
            po_id: id,
            lines: body.lines,
            supplier_ref: body.supplier_ref.clone(),
        },
    )
    .map_err(bad_request)?;
    let invoice = get_ap_invoice(&conn, ap_id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("AP invoice", ap_id))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(invoice).expect("serializable")),
    ))
}

async fn list_ap_invoices_handler(
    State(db): State<SharedDb>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let invoices = list_ap_invoices(&conn).map_err(server_error)?;
    Ok(Json(serde_json::to_value(invoices).expect("serializable")))
}

async fn get_ap_invoice_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let invoice = get_ap_invoice(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("AP invoice", id))?;
    Ok(Json(serde_json::to_value(invoice).expect("serializable")))
}

async fn override_ap_match_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
    Json(body): Json<OverrideMatch>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    override_ap_match(&conn, id, &body.reason, &body.by).map_err(bad_request)?;
    let invoice = get_ap_invoice(&conn, id)
        .map_err(server_error)?
        .ok_or_else(|| not_found("AP invoice", id))?;
    Ok(Json(serde_json::to_value(invoice).expect("serializable")))
}

async fn pay_ap_invoice_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
    Json(body): Json<RecordPayment>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let mut conn = db.lock().expect("db lock");
    let payment_id = pay_ap_invoice_attributed(
        &mut conn,
        id,
        body.amount_cents,
        body.actor.as_deref(),
        body.approver.as_deref(),
    )
    .map_err(bad_request)?;
    let payment = list_ap_payments_for_invoice(&conn, id)
        .map_err(server_error)?
        .into_iter()
        .find(|p| p.id == payment_id)
        .ok_or_else(|| not_found("AP payment", payment_id))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(payment).expect("serializable")),
    ))
}

async fn list_ap_payments_handler(
    State(db): State<SharedDb>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let payments = list_ap_payments(&conn).map_err(server_error)?;
    Ok(Json(serde_json::to_value(payments).expect("serializable")))
}

// ---------------------------------------------------------------------------
// Controls (ENTERPRISE_GAPS.md §6/§7): fiscal periods, users/roles,
// and the payment approval threshold.
// ---------------------------------------------------------------------------

async fn list_fiscal_periods_handler(
    State(db): State<SharedDb>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let periods = list_fiscal_periods(&conn).map_err(server_error)?;
    Ok(Json(serde_json::to_value(periods).expect("serializable")))
}

async fn create_fiscal_period_handler(
    State(db): State<SharedDb>,
    Json(body): Json<CreateFiscalPeriod>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let id = create_fiscal_period(&conn, &body.name, &body.start_date, &body.end_date)
        .map_err(bad_request)?;
    let period = list_fiscal_periods(&conn)
        .map_err(server_error)?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| not_found("fiscal period", id))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(period).expect("serializable")),
    ))
}

async fn close_fiscal_period_handler(
    State(db): State<SharedDb>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    close_fiscal_period(&conn, id).map_err(bad_request)?;
    let period = list_fiscal_periods(&conn)
        .map_err(server_error)?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| not_found("fiscal period", id))?;
    Ok(Json(serde_json::to_value(period).expect("serializable")))
}

async fn list_users_handler(
    State(db): State<SharedDb>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let users = list_users(&conn).map_err(server_error)?;
    Ok(Json(serde_json::to_value(users).expect("serializable")))
}

async fn create_user_handler(
    State(db): State<SharedDb>,
    Json(body): Json<CreateUser>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let id = create_user(&conn, &body.name, &body.role).map_err(bad_request)?;
    let user = list_users(&conn)
        .map_err(server_error)?
        .into_iter()
        .find(|u| u.id == id)
        .ok_or_else(|| not_found("user", id))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(user).expect("serializable")),
    ))
}

async fn list_control_settings_handler(
    State(db): State<SharedDb>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    let settings = list_control_settings(&conn).map_err(server_error)?;
    Ok(Json(serde_json::to_value(settings).expect("serializable")))
}

async fn set_approval_threshold_handler(
    State(db): State<SharedDb>,
    Json(body): Json<SetApprovalThreshold>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let conn = db.lock().expect("db lock");
    set_approval_threshold(&conn, body.threshold_cents).map_err(bad_request)?;
    let settings = list_control_settings(&conn).map_err(server_error)?;
    Ok(Json(serde_json::to_value(settings).expect("serializable")))
}

#[tokio::main]
async fn main() {
    let db_path = std::env::var("ERPCRAFT_DB").unwrap_or_else(|_| "erpcraft.db".to_string());
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    let conn = open_db(&db_path).expect("open ledger database");
    erpcraft_core::seed_mrp_demo_if_empty(&conn).expect("seed MRP demo scenario");
    erpcraft_core::seed_procurement_if_empty(&conn).expect("seed procure-to-pay data");
    let db: SharedDb = Arc::new(Mutex::new(conn));

    let app = Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/accounts", get(accounts))
        .route("/accounts/{code}/entries", get(account_entries_handler))
        .route("/trial-balance", get(trial_balance_handler))
        .route("/journal-entries", get(list_entries_handler))
        .route("/journal-entries", post(post_entry_handler))
        .route("/journal-entries/{id}/audit", get(audit_entry_handler))
        .route("/audit-trail", get(audit_trail_handler))
        .route("/customers", get(customers))
        .route("/items", get(items))
        .route("/quotes", get(list_quotes_handler))
        .route("/quotes", post(create_quote_handler))
        .route("/quotes/{id}", get(get_quote_handler))
        .route("/quotes/{id}/send", post(send_quote_handler))
        .route("/quotes/{id}/accept", post(accept_quote_handler))
        .route("/quotes/{id}/decline", post(decline_quote_handler))
        .route("/quotes/{id}/expire", post(expire_quote_handler))
        .route("/orders", get(list_orders_handler))
        .route("/orders", post(create_order_handler))
        .route("/orders/{id}", get(get_order_handler))
        .route("/orders/{id}/fulfil", post(fulfil_order_handler))
        .route("/orders/{id}/cancel", post(cancel_order_handler))
        .route("/invoices", get(list_invoices_handler))
        .route("/invoices", post(create_invoice_handler))
        .route("/invoices/{id}", get(get_invoice_handler))
        .route("/invoices/{id}/issue", post(issue_invoice_handler))
        .route("/invoices/{id}/void", post(void_invoice_handler))
        .route(
            "/invoices/{id}/payments",
            get(list_invoice_payments_handler),
        )
        .route("/invoices/{id}/payments", post(record_payment_handler))
        .route("/invoices/{id}/returns", post(record_return_handler))
        .route("/payments", get(list_payments_handler))
        .route("/config-models", get(list_config_models_handler))
        .route("/config-models", post(create_config_model_handler))
        .route("/config-models/{id}", get(get_config_model_handler))
        .route(
            "/config-models/{id}/characteristics",
            post(add_characteristic_handler),
        )
        .route(
            "/config-models/{id}/components",
            post(create_component_handler),
        )
        .route(
            "/config-models/{id}/dependencies",
            post(create_dependency_handler),
        )
        .route("/config-models/{id}/evaluate", post(evaluate_handler))
        .route("/suppliers", get(list_suppliers_handler))
        .route("/suppliers", post(create_supplier_handler))
        .route("/purchase-requisitions", get(list_requisitions_handler))
        .route("/purchase-requisitions", post(create_requisition_handler))
        .route("/purchase-requisitions/{id}", get(get_requisition_handler))
        .route(
            "/purchase-requisitions/{id}/approve",
            post(approve_requisition_handler),
        )
        .route(
            "/purchase-requisitions/{id}/reject",
            post(reject_requisition_handler),
        )
        .route(
            "/purchase-requisitions/{id}/cancel",
            post(cancel_requisition_handler),
        )
        .route("/purchase-orders", get(list_procurement_orders_handler))
        .route("/purchase-orders", post(create_purchase_order_handler))
        .route("/purchase-orders/{id}", get(get_procurement_order_handler))
        .route(
            "/purchase-orders/{id}/cancel",
            post(cancel_purchase_order_handler),
        )
        .route(
            "/purchase-orders/{id}/receipts",
            post(receive_goods_handler),
        )
        .route(
            "/purchase-orders/{id}/invoices",
            post(record_ap_invoice_handler),
        )
        .route("/goods-receipts", get(list_goods_receipts_handler))
        .route("/ap-invoices", get(list_ap_invoices_handler))
        .route("/ap-invoices/{id}", get(get_ap_invoice_handler))
        .route(
            "/ap-invoices/{id}/override-match",
            post(override_ap_match_handler),
        )
        .route("/ap-invoices/{id}/payments", post(pay_ap_invoice_handler))
        .route("/ap-payments", get(list_ap_payments_handler))
        .route("/fiscal-periods", get(list_fiscal_periods_handler))
        .route("/fiscal-periods", post(create_fiscal_period_handler))
        .route(
            "/fiscal-periods/{id}/close",
            post(close_fiscal_period_handler),
        )
        .route("/users", get(list_users_handler))
        .route("/users", post(create_user_handler))
        .route("/control-settings", get(list_control_settings_handler))
        .route(
            "/control-settings/approval-threshold",
            post(set_approval_threshold_handler),
        )
        .with_state(db);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind listener");
    println!(
        "ERPCraft (demo company: {DEMO_COMPANY}) listening on http://127.0.0.1:{port} — db: {db_path}"
    );
    axum::serve(listener, app).await.expect("serve");
}
