//! `GET /api/sessions/{id}/trail`: every step a session of this machine took since this daemon
//! started, oldest first, as `{steps: [`[`crate::sessions::brief::Mark`]`]}`. The sessions module
//! hides the gateway key, session tokens, token-shaped words and the password in any URL as each
//! hook arrives. A session of another machine, or one that is gone, is `404`.

use axum::Json;
use axum::extract::{Path, State};
use foundry_proto::ids::SessionId;
use serde_json::{Value, json};

use super::{Api, ApiError, parse_id};

pub(super) async fn trail(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let session = parse_id(&id, "session", SessionId)?;
    let steps = api
        .sessions
        .trail(session)
        .ok_or_else(|| ApiError::NotFound(format!("no session {id:?} here")))?;
    Ok(Json(json!({ "steps": steps })))
}
