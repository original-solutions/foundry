//! Adds and removes projects, through the acting coordinator as every shared-settings change
//! goes, so a project added on one machine shows on every machine.
//!
//! - `POST /api/projects` `{name, repo, id?}`: adds a project with its name and repo trimmed,
//!   `201 {id}`. The id is the one the caller sent, else a new one. A caller that sends its own
//!   id and sends the add again after an answer that came too late sends the same edit, so a
//!   retry cannot make a second project. A name that is blank or over [`NAME_AT_MOST`]
//!   characters, or a repo that is blank or holds whitespace or a NUL, is `422`. A name another
//!   project has, compared trimmed and in any case, or an id another project has, is `409`.
//! - `POST /api/projects/{id}/remove`: removes a project, `204`. An id the settings do not hold
//!   is `404`. A project that a live workspace this machine knows of, from any machine, was made
//!   from is `409`, naming each such workspace.
//!
//! Both checks read this machine's store, so they hold for what it has heard. The coordinator
//! orders edits and checks none: a workspace still being made, or one whose records have not
//! reached this machine, does not stop a remove, and two machines that add one name before
//! hearing of each other's add both go through.
//!
//! Nothing is sent to the coordinator when a request is refused. An edit the coordinator could
//! not make answers as an attention dismissal does ([`edit_status`]). No answer names a repo:
//! any message that could is passed through [`redact`](crate::secrets::redact).

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use foundry_proto::ids::ProjectId;
use foundry_proto::settings::{Edit, Project};
use serde::Deserialize;

use super::{ApiError, Body, created, on_store, parse_id, unprocessable};
use crate::attention::edit_status;
use crate::coordinator::Coordinator;
use crate::store::Store;

/// The most characters a project's name holds.
pub const NAME_AT_MOST: usize = 100;

/// What the project routes act on: the coordinator that sends each edit and the store whose
/// settings and workspaces say whether to send it.
struct Projects {
    coordinator: Arc<Coordinator>,
    store: Arc<Mutex<Store>>,
}

/// `POST /api/projects` and `POST /api/projects/{id}/remove`. The caller guards them as it
/// guards the rest of the API.
pub fn router(coordinator: Arc<Coordinator>, store: Arc<Mutex<Store>>) -> Router {
    Router::new()
        .route("/api/projects", post(add))
        .route("/api/projects/{id}/remove", post(remove))
        .with_state(Arc::new(Projects { coordinator, store }))
}

#[derive(Deserialize)]
struct Add {
    name: String,
    repo: String,
    id: Option<ProjectId>,
}

async fn add(
    State(projects): State<Arc<Projects>>,
    Body(add): Body<Add>,
) -> Result<Response, ApiError> {
    let (name, repo) = (add.name.trim(), add.repo.trim());
    if name.is_empty() {
        return Err(unprocessable("a project needs a name".to_owned()));
    }
    if name.chars().count() > NAME_AT_MOST {
        return Err(unprocessable(format!(
            "a project's name is at most {NAME_AT_MOST} characters"
        )));
    }
    if repo.is_empty() {
        return Err(unprocessable("a project needs a repo URL".to_owned()));
    }
    if repo.contains(|c: char| c.is_whitespace() || c == '\0') {
        return Err(unprocessable(
            "a repo URL holds no spaces, tabs, line breaks or NULs".to_owned(),
        ));
    }
    let settings = on_store(&projects.store, |store| store.settings()).await?;
    let wanted = name.to_lowercase();
    if let Some(held) = settings
        .projects
        .iter()
        .find(|p| p.name.trim().to_lowercase() == wanted)
    {
        return Err(conflict(format!(
            "a project named {:?} already exists",
            held.name
        )));
    }
    let id = add.id.unwrap_or_else(ProjectId::random);
    if settings.projects.iter().any(|p| p.id == id) {
        return Err(conflict(format!(
            "a project with the id {id} already exists"
        )));
    }
    let project = Project {
        id,
        name: name.to_owned(),
        repo: repo.to_owned(),
    };
    edit(&projects, Edit::SetProject { project }).await?;
    Ok(created(id))
}

async fn remove(
    State(projects): State<Arc<Projects>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id, "project", ProjectId)?;
    let (settings, live) = on_store(&projects.store, |store| {
        Ok((store.settings()?, store.live_workspaces()?))
    })
    .await?;
    let Some(project) = settings.projects.iter().find(|p| p.id == id) else {
        return Err(ApiError::NotFound(format!("no project {id} here")));
    };
    let holding: Vec<String> = live
        .iter()
        .filter(|w| w.body.project == id)
        .map(|w| {
            let machine = settings
                .machines
                .iter()
                .find(|m| m.id == w.owner)
                .map_or_else(|| w.owner.to_string(), |m| m.name.clone());
            format!("{:?} on {machine}", w.body.name)
        })
        .collect();
    if !holding.is_empty() {
        return Err(conflict(format!(
            "{} still has live workspaces: {}; archive them first",
            project.name,
            holding.join(", ")
        )));
    }
    edit(&projects, Edit::RemoveProject { id }).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Sends `edit` through the coordinator, answering a failure as a dismissal's is answered.
async fn edit(projects: &Projects, edit: Edit) -> Result<(), ApiError> {
    match projects.coordinator.edit(edit).await {
        Ok(_) => Ok(()),
        Err(e) => Err(ApiError::Rejected(edit_status(&e), e.to_string())),
    }
}

fn conflict(message: String) -> ApiError {
    ApiError::Rejected(StatusCode::CONFLICT, message)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use foundry_proto::ids::{MachineId, ProjectId, WorkspaceId};
    use foundry_proto::messages::{PROTOCOL, PeerMessage};
    use foundry_proto::record::{AnyRecord, Record, Workspace};
    use foundry_proto::settings::{Machine, Project, SharedSettings};
    use futures::future::BoxFuture;
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::sync::{Conn, Dialer, Peers};

    struct NoDial;

    impl Dialer for NoDial {
        fn dial(&self, _: &Machine) -> BoxFuture<'static, std::io::Result<Conn>> {
            Box::pin(async { Err(std::io::Error::other("no peers in these tests")) })
        }
    }

    /// The project routes of one machine, served on a loopback port.
    struct Served {
        _data: tempfile::TempDir,
        store: Arc<Mutex<Store>>,
        peers: Peers,
        addr: std::net::SocketAddr,
        _server: tokio::task::JoinHandle<()>,
    }

    impl Served {
        /// A machine that acts alone, holding the settings `settings` makes.
        async fn new(settings: impl FnOnce(MachineId, &mut SharedSettings)) -> Self {
            let data = tempfile::tempdir().unwrap();
            let me = MachineId::random();
            let mut store = Store::open(data.path().join("foundry.db")).unwrap();
            let mut held = store.settings().unwrap();
            held.seq += 1;
            settings(me, &mut held);
            store.apply_settings(held).unwrap();
            let store = Arc::new(Mutex::new(store));
            let peers = Peers::new(me, store.clone(), Arc::new(NoDial));
            let coordinator = Arc::new(Coordinator::new(me, store.clone(), peers.clone()));
            peers.coordinate(coordinator.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = router(coordinator, store.clone());
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                _data: data,
                store,
                peers,
                addr,
                _server: server,
            }
        }

        /// `(status, body)` of `POST path`, with `body` as JSON when there is one.
        async fn post(&self, path: &str, body: Option<Value>) -> (u16, String) {
            let mut stream = tokio::net::TcpStream::connect(self.addr).await.unwrap();
            let body = body.map(|b| b.to_string()).unwrap_or_default();
            let request = format!(
                "POST {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                self.addr,
                body.len()
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut response = Vec::new();
            tokio::time::timeout(Duration::from_secs(20), stream.read_to_end(&mut response))
                .await
                .expect("the routes answer in time")
                .unwrap();
            let response = String::from_utf8(response).unwrap();
            let (head, body) = response.split_once("\r\n\r\n").unwrap();
            (
                head.split(' ').nth(1).unwrap().parse().unwrap(),
                body.to_owned(),
            )
        }

        async fn add(&self, name: &str, repo: &str) -> (u16, String) {
            self.post("/api/projects", Some(json!({ "name": name, "repo": repo })))
                .await
        }

        fn settings(&self) -> SharedSettings {
            self.store.lock().unwrap().settings().unwrap()
        }

        /// Stores a workspace `owner` made from `project`, live or archived.
        fn workspace(&self, owner: MachineId, project: ProjectId, name: &str, archived: bool) {
            let mut store = self.store.lock().unwrap();
            let version = store.versions().unwrap().get(&owner).copied().unwrap_or(0) + 1;
            let record = AnyRecord::Workspace(Record {
                owner,
                version,
                deleted: archived,
                body: Workspace {
                    id: WorkspaceId::random(),
                    project,
                    name: name.into(),
                    branch: name.replace(' ', "-"),
                    base: "main".into(),
                    setup: None,
                },
            });
            store.apply(&record, owner).unwrap();
        }

        /// `peer` says hello and sends nothing, so it is heard from, then its link drops: it
        /// acts, and an edit has nowhere to go.
        async fn hear(&self, peer: MachineId) {
            let (ours, theirs) = Conn::pair();
            let accepting = tokio::spawn({
                let peers = self.peers.clone();
                async move { peers.accept(ours).await }
            });
            let hello = PeerMessage::Hello {
                protocol: PROTOCOL,
                machine: peer,
                versions: BTreeMap::new(),
            };
            let nothing = PeerMessage::Records {
                protocol: PROTOCOL,
                records: Vec::new(),
            };
            for message in [hello, nothing] {
                theirs
                    .send
                    .send(serde_json::to_string(&message).unwrap())
                    .await
                    .unwrap();
            }
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while !self.peers.online().contains(&peer) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the peer was not heard"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            drop(theirs);
            accepting.await.unwrap();
        }
    }

    fn project(name: &str, repo: &str) -> Project {
        Project {
            id: ProjectId::random(),
            name: name.into(),
            repo: repo.into(),
        }
    }

    fn id_of(body: &str) -> ProjectId {
        let json: Value = serde_json::from_str(body).unwrap();
        ProjectId(json["id"].as_str().unwrap().parse().unwrap())
    }

    fn error_of(body: &str) -> String {
        let json: Value = serde_json::from_str(body).unwrap();
        json["error"].as_str().unwrap().to_owned()
    }

    // --- adding ---

    #[tokio::test]
    async fn a_project_added_on_the_acting_machine_is_in_the_settings_trimmed() {
        let served = Served::new(|_, _| {}).await;
        let (status, body) = served
            .add("  foundry ", " https://github.com/o/foundry.git\n")
            .await;
        assert_eq!(status, 201, "{body}");
        let json: Value = serde_json::from_str(&body).unwrap();
        let id: ProjectId = ProjectId(json["id"].as_str().unwrap().parse().unwrap());
        assert_eq!(
            served.settings().projects,
            [Project {
                id,
                name: "foundry".into(),
                repo: "https://github.com/o/foundry.git".into(),
            }]
        );
    }

    #[tokio::test]
    async fn each_project_added_gets_an_id_of_its_own() {
        let served = Served::new(|_, _| {}).await;
        let (first, _) = served.add("one", "git@github.com:o/one.git").await;
        let (second, _) = served.add("two", "git@github.com:o/two.git").await;
        assert_eq!((first, second), (201, 201));
        let projects = served.settings().projects;
        assert_eq!(projects.len(), 2);
        assert_ne!(projects[0].id, projects[1].id);
    }

    #[tokio::test]
    async fn an_add_under_the_callers_id_keeps_that_id() {
        let served = Served::new(|_, _| {}).await;
        let id = ProjectId::random();
        let (status, body) = served
            .post(
                "/api/projects",
                Some(json!({ "name": "api", "repo": "/srv/api.git", "id": id })),
            )
            .await;
        assert_eq!(status, 201, "{body}");
        assert_eq!(id_of(&body), id);
        assert_eq!(
            served.settings().projects,
            [Project {
                id,
                name: "api".into(),
                repo: "/srv/api.git".into(),
            }]
        );
    }

    #[tokio::test]
    async fn an_add_under_an_id_another_project_has_is_409_and_sends_nothing() {
        let held = project("foundry", "/srv/foundry.git");
        let id = held.id;
        let served = Served::new({
            let held = held.clone();
            move |_, settings| settings.projects.push(held)
        })
        .await;
        let seq = served.settings().seq;
        let (status, body) = served
            .post(
                "/api/projects",
                Some(json!({ "name": "other", "repo": "/srv/other.git", "id": id })),
            )
            .await;
        assert_eq!(status, 409, "{body}");
        assert!(error_of(&body).contains(&id.to_string()), "{body}");
        assert_eq!(served.settings().seq, seq);
        assert_eq!(served.settings().projects, [held]);
    }

    #[tokio::test]
    async fn a_name_of_100_characters_is_taken_and_one_of_101_is_not() {
        let served = Served::new(|_, _| {}).await;
        let (status, body) = served.add(&"é".repeat(100), "/srv/a.git").await;
        assert_eq!(status, 201, "{body}");
        let (status, body) = served.add(&"x".repeat(101), "/srv/b.git").await;
        assert_eq!(status, 422, "{body}");
    }

    #[tokio::test]
    async fn a_blank_or_long_name_or_a_blank_or_spaced_repo_is_422_and_sends_nothing() {
        let served = Served::new(|_, _| {}).await;
        let seq = served.settings().seq;
        let long = "n".repeat(NAME_AT_MOST + 1);
        let cases = [
            ("", "/srv/a.git"),
            ("   ", "/srv/a.git"),
            (long.as_str(), "/srv/a.git"),
            ("a", ""),
            ("a", " \t "),
            ("a", "https://github.com/o/my repo.git"),
            ("a", "/srv/a\tb.git"),
            ("a", "/srv/a\u{0}b.git"),
        ];
        for (name, repo) in cases {
            let (status, body) = served.add(name, repo).await;
            assert_eq!(status, 422, "{name:?} {repo:?}: {body}");
            assert!(!error_of(&body).is_empty(), "{body}");
        }
        assert_eq!(served.settings().seq, seq);
        assert!(served.settings().projects.is_empty());
    }

    #[tokio::test]
    async fn a_name_another_project_has_in_any_case_or_spacing_is_409_and_sends_nothing() {
        let held = project("Foundry", "/srv/foundry.git");
        let served = Served::new({
            let held = held.clone();
            move |_, settings| settings.projects.push(held)
        })
        .await;
        let seq = served.settings().seq;
        let (status, body) = served.add("  fOUNDRY ", "/srv/other.git").await;
        assert_eq!(status, 409, "{body}");
        assert!(error_of(&body).contains("Foundry"), "{body}");
        assert_eq!(served.settings().seq, seq);
        assert_eq!(served.settings().projects, [held]);
    }

    #[tokio::test]
    async fn a_repo_password_never_appears_in_an_answer() {
        let served = Served::new(|_, settings| {
            settings
                .projects
                .push(project("taken", "https://tom:hunter2@github.com/o/t.git"));
        })
        .await;
        let refused = [
            ("taken", "https://tom:hunter2@github.com/o/new.git"),
            ("", "https://tom:hunter2@github.com/o/new.git"),
            ("new", "https://tom:hunter2@github.com/o/my repo.git"),
        ];
        for (name, repo) in refused {
            let (status, body) = served.add(name, repo).await;
            assert!(status >= 400, "{name:?}: {status} {body}");
            assert!(!body.contains("hunter2"), "{body}");
        }
        let (status, body) = served
            .add("new", "https://tom:hunter2@github.com/o/new.git")
            .await;
        assert_eq!(status, 201, "{body}");
        assert!(!body.contains("hunter2"), "{body}");
    }

    #[tokio::test]
    async fn an_add_the_coordinator_cannot_send_answers_503_without_the_repo() {
        let peer = MachineId::random();
        let served = Served::new(|me, settings| {
            settings.machines = vec![Machine {
                id: peer,
                name: "trilogy-mac".into(),
                address: "100.64.0.2".into(),
            }];
            settings.priority = vec![peer, me];
        })
        .await;
        // The peer is heard from, so it acts, then its link drops: the edit has nowhere to go.
        served.hear(peer).await;
        let (status, body) = served
            .add("foundry", "https://tom:hunter2@github.com/o/foundry.git")
            .await;
        assert_eq!(status, 503, "{body}");
        assert!(error_of(&body).contains(&peer.to_string()), "{body}");
        for value in ["hunter2", "github.com"] {
            assert!(!body.contains(value), "{value} in {body}");
        }
        assert!(served.settings().projects.is_empty());
    }

    // --- removing ---

    #[tokio::test]
    async fn removing_a_project_takes_it_out_of_the_settings() {
        let held = project("foundry", "/srv/foundry.git");
        let id = held.id;
        let served = Served::new(move |_, settings| settings.projects.push(held)).await;
        let (status, body) = served
            .post(&format!("/api/projects/{id}/remove"), None)
            .await;
        assert_eq!(status, 204, "{body}");
        assert!(served.settings().projects.is_empty());
    }

    #[tokio::test]
    async fn removing_a_project_the_settings_do_not_hold_is_404_and_sends_nothing() {
        let served = Served::new(|_, settings| {
            settings
                .projects
                .push(project("foundry", "/srv/foundry.git"));
        })
        .await;
        let seq = served.settings().seq;
        for id in [ProjectId::random().to_string(), "nonsense".to_owned()] {
            let (status, body) = served
                .post(&format!("/api/projects/{id}/remove"), None)
                .await;
            assert_eq!(status, 404, "{id}: {body}");
            assert!(!error_of(&body).is_empty(), "{body}");
        }
        assert_eq!(served.settings().seq, seq);
        assert_eq!(served.settings().projects.len(), 1);
    }

    #[tokio::test]
    async fn removing_a_project_with_live_workspaces_is_409_naming_each_and_keeps_it() {
        let held = project("foundry", "/srv/foundry.git");
        let id = held.id;
        let peer = MachineId::random();
        let served = Served::new(move |_, settings| {
            settings.machines = vec![Machine {
                id: peer,
                name: "trilogy-mac".into(),
                address: "100.64.0.2".into(),
            }];
            settings.projects.push(held);
        })
        .await;
        let gone = MachineId::random();
        served.workspace(peer, id, "fix login", false);
        served.workspace(peer, id, "tidy readme", false);
        served.workspace(gone, id, "left behind", false);
        served.workspace(peer, id, "old work", true);
        served.workspace(peer, ProjectId::random(), "elsewhere", false);
        let seq = served.settings().seq;

        let (status, body) = served
            .post(&format!("/api/projects/{id}/remove"), None)
            .await;
        assert_eq!(status, 409, "{body}");
        let error = error_of(&body);
        for named in ["fix login", "tidy readme", "left behind"] {
            assert!(error.contains(named), "{named} not in {error}");
        }
        assert!(error.contains("on trilogy-mac"), "{error}");
        // A machine the settings no longer list is named by its id.
        assert!(
            error.contains(&format!("\"left behind\" on {gone}")),
            "{error}"
        );
        for unnamed in ["old work", "elsewhere"] {
            assert!(!error.contains(unnamed), "{unnamed} in {error}");
        }
        assert_eq!(served.settings().seq, seq);
        assert_eq!(served.settings().projects.len(), 1);
    }

    #[tokio::test]
    async fn a_project_whose_workspaces_are_all_archived_can_be_removed() {
        let held = project("foundry", "/srv/foundry.git");
        let id = held.id;
        let served = Served::new(move |_, settings| settings.projects.push(held)).await;
        served.workspace(MachineId::random(), id, "old work", true);
        let (status, body) = served
            .post(&format!("/api/projects/{id}/remove"), None)
            .await;
        assert_eq!(status, 204, "{body}");
        assert!(served.settings().projects.is_empty());
    }

    #[tokio::test]
    async fn a_remove_the_coordinator_cannot_send_answers_503_and_keeps_the_project() {
        let held = project("foundry", "https://tom:hunter2@github.com/o/foundry.git");
        let id = held.id;
        let peer = MachineId::random();
        let served = Served::new(move |me, settings| {
            settings.machines = vec![Machine {
                id: peer,
                name: "trilogy-mac".into(),
                address: "100.64.0.2".into(),
            }];
            settings.priority = vec![peer, me];
            settings.projects.push(held);
        })
        .await;
        served.hear(peer).await;
        let (status, body) = served
            .post(&format!("/api/projects/{id}/remove"), None)
            .await;
        assert_eq!(status, 503, "{body}");
        assert!(!body.contains("hunter2"), "{body}");
        assert_eq!(served.settings().projects.len(), 1);
    }
}
