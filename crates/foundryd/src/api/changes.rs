//! `GET /api/workspaces/{id}/changes`: what a workspace of this machine has changed since it
//! left its base, as [`crate::workspaces::Changes`]. Every path, patch and commit subject is
//! passed through [`redact`], so a URL with a password in it shows without the password.
//! `GET /api/workspaces/{id}/commits/{hash}`: what one commit on its branch changed, shown the
//! same way.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use foundry_proto::ids::WorkspaceId;

use super::{Api, ApiError, parse_id, redact};
use crate::workspaces::Changes;

pub(super) async fn changes(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id, "workspace", WorkspaceId)?;
    let changes = api.workspaces.changes(id).await?;
    Ok(Json(shown(changes)).into_response())
}

pub(super) async fn commit(
    State(api): State<Api>,
    Path((id, hash)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id, "workspace", WorkspaceId)?;
    let changes = api.workspaces.commit(id, &hash).await?;
    Ok(Json(shown(changes)).into_response())
}

/// `changes` with [`redact`] applied to every path, patch and commit subject.
fn shown(mut changes: Changes) -> Changes {
    for commit in &mut changes.commits {
        commit.subject = redact(&commit.subject);
    }
    for file in &mut changes.files {
        file.path = redact(&file.path);
        file.from = file.from.as_deref().map(redact);
        file.patch = file.patch.as_deref().map(redact);
    }
    changes
}

#[cfg(test)]
mod tests {
    use crate::workspaces::{Commit, FileChange, FileStatus};

    use super::*;

    /// Git never puts `//` in a path, so no repo can make these; the guard is tested on its own.
    #[test]
    fn every_path_patch_and_subject_is_shown_without_url_passwords() {
        let url = |what: &str| format!("{what} https://bot:hunter2@example.com/r");
        let changes = Changes {
            base: "main".into(),
            commits: vec![Commit {
                hash: "abc1234".into(),
                subject: url("mirror to"),
            }],
            files: vec![FileChange {
                path: url("to"),
                from: Some(url("from")),
                status: FileStatus::Renamed,
                added: Some(1),
                removed: Some(0),
                uncommitted: false,
                patch: Some(url("+clone")),
            }],
        };
        let shown = format!("{:?}", shown(changes));
        assert!(!shown.contains("hunter2"), "{shown}");
        assert_eq!(
            shown.matches("https://***@example.com/r").count(),
            4,
            "{shown}"
        );
    }
}
