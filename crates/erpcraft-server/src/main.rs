//! ERPCraft HTTP server — a thin Axum shell over `erpcraft-core`.
//!
//! Day 1 surface: health/root, chart of accounts, trial balance, and journal
//! posting/read-back. Every write goes through the ledger core, whose
//! invariants are enforced by the database itself.

use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use erpcraft_core::rusqlite::Connection;
use erpcraft_core::{
    account_count, entry_count, get_entry, list_accounts, list_entries, open_db, post_entry,
    trial_balance, NewEntry, DEMO_COMPANY,
};
use serde::Serialize;
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
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

async fn root() -> Json<RootInfo> {
    Json(RootInfo {
        name: "ERPCraft",
        vision: "A free, open-source, clean-room ERP in Rust — AI-native from day one.",
        status: "day-1 alpha: ledger core (chart of accounts, double-entry journal, trial balance)",
        demo_company: DEMO_COMPANY,
    })
}

async fn health(State(db): State<SharedDb>) -> Json<Health> {
    let conn = db.lock().expect("db lock");
    Json(Health {
        status: "ok",
        accounts: account_count(&conn).unwrap_or(-1),
        journal_entries: entry_count(&conn).unwrap_or(-1),
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
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: e.to_string(),
            }),
        )),
    }
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
        .with_state(db);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind listener");
    println!(
        "ERPCraft (demo company: {DEMO_COMPANY}) listening on http://127.0.0.1:{port} — db: {db_path}"
    );
    axum::serve(listener, app).await.expect("serve");
}
