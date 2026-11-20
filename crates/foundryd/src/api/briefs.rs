//! `GET /api/briefs`: what each session of this machine is for and doing, as
//! [`crate::sessions::brief::Brief`] by session id. The sessions module hides the gateway key,
//! session tokens, token-shaped words and the password in any URL as each hook arrives.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::State;

use super::Api;
use crate::sessions::brief::Brief;

pub(super) async fn briefs(State(api): State<Api>) -> Json<BTreeMap<String, Brief>> {
    let briefs = api.sessions.briefs().into_iter();
    Json(briefs.map(|(id, brief)| (id.to_string(), brief)).collect())
}
