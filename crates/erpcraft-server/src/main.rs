//! ERPCraft HTTP server — a thin Axum shell over `erpcraft-core`.
//!
//! Day 1 surface: health/root, chart of accounts, trial balance, and journal
//! posting/read-back. Day 2 adds the order-to-cash documents (customers,
//! items, quotes, sales orders, invoices, payments) — documents only, no GL
//! postings. Every write goes through the ledger core; invariant violations
//! come back as 400 with the core's plain-language message.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use erpcraft_core::rusqlite::Connection;
use erpcraft_core::{
    accept_quote, account_count, cancel_order, create_invoice_from_order, create_order_from_quote,
    create_quote, customer_count, decline_quote, entry_count, expire_quote, fulfil_order,
    get_entry, get_invoice, get_order, get_quote, invoice_count, issue_invoice, list_accounts,
    list_customers, list_entries, list_invoices, list_items, list_orders, list_payments,
    list_payments_for_invoice, list_quotes, open_db, order_count, payment_count, post_entry,
    quote_count, record_payment, send_quote, trial_balance, void_invoice, LedgerError, NewEntry,
    NewQuote, DEMO_COMPANY,
};
use serde::{Deserialize, Serialize};
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
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
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
}

/// A refused state transition / validation failure is the client's error:
/// 400 with the core's plain-language message.
fn bad_request(e: LedgerError) -> (StatusCode, Json<ErrorBody>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorBody {
            error: e.to_string(),
        }),
    )
}

fn not_found(what: &str, id: i64) -> (StatusCode, Json<ErrorBody>) {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorBody {
            error: format!("no {what} with id {id}"),
        }),
    )
}

fn server_error(e: LedgerError) -> (StatusCode, Json<ErrorBody>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorBody {
            error: e.to_string(),
        }),
    )
}

async fn root() -> Json<RootInfo> {
    Json(RootInfo {
        name: "ERPCraft",
        vision: "A free, open-source, clean-room ERP in Rust — AI-native from day one.",
        status: "day-2 alpha: ledger core + order-to-cash documents (quotes, orders, invoices, payments — no GL postings yet)",
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

async fn list_entries_handler(
    State(db): State<SharedDb>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let conn = db.lock().expect("db lock");
    let entries = list_entries(&conn).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "journal_entries": entries })))
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
                Json(ErrorBody {
                    error: "posted but could not be read back".to_string(),
                }),
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
    let payment_id = record_payment(&mut conn, id, body.amount_cents).map_err(bad_request)?;
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

#[tokio::main]
async fn main() {
    let db_path = std::env::var("ERPCRAFT_DB").unwrap_or_else(|_| "erpcraft.db".to_string());
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    let conn = open_db(&db_path).expect("open ledger database");
    let db: SharedDb = Arc::new(Mutex::new(conn));

    let app = Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/accounts", get(accounts))
        .route("/trial-balance", get(trial_balance_handler))
        .route("/journal-entries", get(list_entries_handler))
        .route("/journal-entries", post(post_entry_handler))
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
        .route("/payments", get(list_payments_handler))
        .with_state(db);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind listener");
    println!(
        "ERPCraft (demo company: {DEMO_COMPANY}) listening on http://127.0.0.1:{port} — db: {db_path}"
    );
    axum::serve(listener, app).await.expect("serve");
}
