//! `GET /api/sessions/{id}/trail`: every step a session of this machine took since this daemon
//! started, oldest first, as `{steps: [`[`crate::sessions::brief::Mark`]`]}`. The sessions module
//! hides the gateway key, session tokens and token-shaped words; [`redact`] hides the password in
//! any URL on top. A session of another machine, or one that is gone, is `404`.

use axum::Json;
use axum::extract::{Path, State};
use foundry_proto::ids::SessionId;
use serde_json::{Value, json};

use super::{Api, ApiError, parse_id, redact};

pub(super) async fn trail(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let session = parse_id(&id, "session", SessionId)?;
    let mut steps = api
        .sessions
        .trail(session)
        .ok_or_else(|| ApiError::NotFound(format!("no session {id:?} here")))?;
    for step in &mut steps {
        step.text = redact(&step.text);
    }
    Ok(Json(json!({ "steps": steps })))
}
