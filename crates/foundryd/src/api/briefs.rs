//! `GET /api/briefs`: what each session of this machine is for and doing, as
//! [`crate::sessions::brief::Brief`] by session id. The sessions module hides the gateway key,
//! session tokens and token-shaped words; [`redact`] hides the password in any URL on top.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::State;

use super::{Api, redact};
use crate::sessions::brief::Brief;

pub(super) async fn briefs(State(api): State<Api>) -> Json<BTreeMap<String, Brief>> {
    let briefs = api.sessions.briefs().into_iter().map(|(id, mut brief)| {
        brief.title = brief.title.as_deref().map(redact);
        if let Some(step) = &mut brief.now {
            step.text = redact(&step.text);
        }
        (id.to_string(), brief)
    });
    Json(briefs.collect())
}
