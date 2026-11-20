//! `GET /api/workspaces/{id}/pr`: where a workspace of this machine stands past it, as
//! [`crate::workspaces::Shipping`]: on `origin` or not, the commits only here, and its pull
//! request. The pull request's title and link are passed through [`redact`].

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use foundry_proto::ids::WorkspaceId;

use super::{Api, ApiError, parse_id, redact};

pub(super) async fn shipping(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id, "workspace", WorkspaceId)?;
    let mut shipping = api.workspaces.shipping(id).await?;
    if let Some(pr) = &mut shipping.pr {
        pr.title = redact(&pr.title);
        pr.url = redact(&pr.url);
    }
    shipping.branch = redact(&shipping.branch);
    Ok(Json(shipping).into_response())
}
