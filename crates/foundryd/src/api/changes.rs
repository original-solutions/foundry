//! `GET /api/workspaces/{id}/changes`: what a workspace of this machine has changed since it
//! left its base, as [`crate::workspaces::Changes`]. Every path, patch and commit subject is
//! passed through [`redact`], so a URL with a password in it shows without the password.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use foundry_proto::ids::WorkspaceId;

use super::{Api, ApiError, parse_id, redact};

pub(super) async fn changes(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id, "workspace", WorkspaceId)?;
    let mut changes = api.workspaces.changes(id).await?;
    for commit in &mut changes.commits {
        *commit = redact(commit);
    }
    for file in &mut changes.files {
        file.path = redact(&file.path);
        file.from = file.from.as_deref().map(redact);
        file.patch = file.patch.as_deref().map(redact);
    }
    Ok(Json(changes).into_response())
}
