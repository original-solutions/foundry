//! `GET /api/machine`: which machine serves the UI, as `{"id": ...}`. Only this machine can end
//! its terminals, so the UI offers ending one only when its record names this machine.

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use super::Api;

pub(super) async fn machine(State(api): State<Api>) -> Json<Value> {
    Json(json!({ "id": api.terminals.me().to_string() }))
}
