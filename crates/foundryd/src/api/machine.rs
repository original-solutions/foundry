//! `GET /api/machine`: which machine serves the UI, as `{"id": ...}`, so the UI can tell this
//! machine's records from another machine's.

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use super::Api;

pub(super) async fn machine(State(api): State<Api>) -> Json<Value> {
    Json(json!({ "id": api.terminals.me().to_string() }))
}
