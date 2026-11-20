//! Raises and clears attention items: what needs a person, with what happened and what to do.
//!
//! An item is not stored. It is read from what holds now, so it is raised once its rule holds
//! and cleared once it no longer does. One read of the list is kept for [`KEPT_FOR`] and
//! answers every poll in that time, so the records are read at most once a second however
//! many tabs poll:
//!
//! | Item | Raised while | Read from |
//! |---|---|---|
//! | Machine offline | a machine in the settings is not heard from for [`OFFLINE_AFTER`] | the peer links |
//! | Session stuck | a session is stuck | its record |
//! | Session waiting for you | a session waits for you | its record |
//! | Session failed | a session failed and its terminal is not closed | its record and its terminal's |
//! | Setup failed | a live workspace's setup terminal exited other than 0 and is not closed | the workspace's record and its terminal's |
//! | Disk low | a machine's newest sample has less free disk than its limit | the sample and the limits |
//! | Setting overwritten | the shared settings hold an overwritten setting | the settings |
//! | Update refused | a machine's newest sample names a refused tag | the sample |
//! | Gateway unhealthy | this machine's read of the gateway fails | [`crate::gateway`] |
//!
//! Every row but the gateway's comes from the synced records and shared settings, so each
//! machine shows the whole fleet's. Records are read as the rest of the UI sees them: an offline
//! machine's sessions and terminals are unreachable, so they raise nothing while its "machine
//! offline" item stands. Another module adds rows through a [`Source`].
//!
//! - `GET /api/attention`: `{"items": [...]}`, each an [`Item`].
//! - `POST /api/attention/{id}/dismiss`: dismisses a "setting overwritten" item through the
//!   coordinator, `204`. Any other item is `409`; an id that names no item is `404`. An edit
//!   that fails answers with its error, which names no setting's value.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use foundry_proto::ids::{MachineId, TerminalId};
use foundry_proto::record::{AnyRecord, Terminal};
use foundry_proto::settings::{Edit, SettingKey, SettingValue, SharedSettings};
use foundry_proto::state::{SessionState, TerminalState};
use serde::Serialize;
use tokio::time::Instant;

use crate::coordinator::{Coordinator, CoordinatorError};
use crate::machine::{Over, over_limits};
use crate::secrets::redact;
use crate::sessions::STUCK_AFTER;
use crate::store::{Store, StoreError};
use crate::sync::{OFFLINE_AFTER, Peers, on_store};

/// How long one read of the list answers every poll. A read loads every record held, so the
/// polls of every open tab share it. With the UI's 2 second poll an item still shows within
/// 3 seconds of what raised it.
pub const KEPT_FOR: Duration = Duration::from_secs(1);

/// What an item is about, one per row of the spec's table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    MachineOffline,
    SessionStuck,
    SessionWaiting,
    SessionFailed,
    SetupFailed,
    GatewayUnhealthy,
    DiskLow,
    SettingOverwritten,
    UpdateRefused,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MachineOffline => "machine_offline",
            Self::SessionStuck => "session_stuck",
            Self::SessionWaiting => "session_waiting",
            Self::SessionFailed => "session_failed",
            Self::SetupFailed => "setup_failed",
            Self::GatewayUnhealthy => "gateway_unhealthy",
            Self::DiskLow => "disk_low",
            Self::SettingOverwritten => "setting_overwritten",
            Self::UpdateRefused => "update_refused",
        })
    }
}

/// One thing that needs a person.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Item {
    /// `<kind>:<what it is about>`, such as `session_waiting:<session id>`, the same on every
    /// read while the item stands.
    pub id: String,
    pub kind: Kind,
    /// The machine it is about.
    pub machine: MachineId,
    /// That machine's name in the shared settings, or its id when it has none.
    pub machine_name: String,
    /// What happened.
    pub what: String,
    /// What to do.
    pub todo: String,
    /// The terminal to open to act on it, when there is one.
    pub terminal: Option<TerminalId>,
}

/// What every source reads: the records held from every owner, the shared settings, the
/// machines heard from, and this machine.
pub struct Fleet {
    pub me: MachineId,
    pub records: Vec<AnyRecord>,
    pub settings: SharedSettings,
    /// This machine and every machine heard from within [`OFFLINE_AFTER`].
    pub online: BTreeSet<MachineId>,
}

impl Fleet {
    /// `machine`'s name in the shared settings, else its id.
    pub fn name(&self, machine: MachineId) -> String {
        self.settings
            .machines
            .iter()
            .find(|m| m.id == machine)
            .map_or_else(|| machine.to_string(), |m| m.name.clone())
    }

    /// An item of `kind` about `about` on `machine`.
    pub fn item(
        &self,
        kind: Kind,
        about: impl fmt::Display,
        machine: MachineId,
        what: String,
        todo: String,
    ) -> Item {
        Item {
            id: format!("{kind}:{about}"),
            kind,
            machine,
            machine_name: self.name(machine),
            what,
            todo,
            terminal: None,
        }
    }
}

/// A module that raises items of its own, read on every list of items.
pub trait Source: Send + Sync {
    fn items(&self, fleet: &Fleet) -> Vec<Item>;
}

/// The items the synced records and shared settings raise: every row but the gateway's.
pub fn fleet_items(fleet: &Fleet) -> Vec<Item> {
    let mut items = Vec::new();
    for machine in &fleet.settings.machines {
        if !fleet.online.contains(&machine.id) {
            items.push(fleet.item(
                Kind::MachineOffline,
                machine.id,
                machine.id,
                format!(
                    "{} has not been heard from for {} seconds",
                    machine.name,
                    OFFLINE_AFTER.as_secs()
                ),
                "Check that it is on, on the tailnet and running foundryd".to_owned(),
            ));
        }
    }

    let mut workspaces = BTreeMap::new();
    let mut terminals: BTreeMap<TerminalId, (bool, &Terminal)> = BTreeMap::new();
    for record in &fleet.records {
        match record {
            AnyRecord::Workspace(r) => {
                workspaces.insert(r.body.id, r);
            }
            AnyRecord::Terminal(r) => {
                terminals.insert(r.body.id, (r.deleted, &r.body));
            }
            _ => {}
        }
    }
    let workspace_name = |id| {
        workspaces
            .get(&id)
            .map_or_else(|| "a workspace".to_owned(), |w| w.body.name.clone())
    };
    let closed = |id| terminals.get(&id).is_some_and(|(deleted, _)| *deleted);
    let exit = |id| match terminals.get(&id).and_then(|(_, t)| t.exit_code) {
        Some(code) => format!(" (exit code {code})"),
        None => String::new(),
    };

    for record in &fleet.records {
        match record {
            AnyRecord::Session(r) if !r.deleted => {
                let session = &r.body;
                let place = format!(
                    "A session in {} on {}",
                    workspace_name(session.workspace),
                    fleet.name(r.owner)
                );
                let (kind, what, todo) = match session.state {
                    SessionState::Stuck => (
                        Kind::SessionStuck,
                        format!(
                            "{place} has shown no output or hook event for {} minutes",
                            STUCK_AFTER.as_secs() / 60
                        ),
                        "Open its terminal and see what it is doing",
                    ),
                    SessionState::WaitingForYou => (
                        Kind::SessionWaiting,
                        format!("{place} is waiting for you"),
                        "Open its terminal and answer it",
                    ),
                    SessionState::Failed if !closed(session.terminal) => (
                        Kind::SessionFailed,
                        format!("{place} failed{}", exit(session.terminal)),
                        "Read its terminal for why, then close it",
                    ),
                    _ => continue,
                };
                let mut item = fleet.item(kind, session.id, r.owner, what, todo.to_owned());
                item.terminal = Some(session.terminal);
                items.push(item);
            }
            AnyRecord::Workspace(r) if !r.deleted => {
                let Some(setup) = r.body.setup else { continue };
                let failed = terminals.get(&setup).is_some_and(|(deleted, t)| {
                    !deleted && t.state == TerminalState::Exited && t.exit_code != Some(0)
                });
                if failed {
                    let mut item = fleet.item(
                        Kind::SetupFailed,
                        r.body.id,
                        r.owner,
                        format!(
                            "The setup of {} on {} failed{}",
                            r.body.name,
                            fleet.name(r.owner),
                            exit(setup)
                        ),
                        "Read its setup terminal, fix the workspace by hand, then close that \
                         terminal"
                            .to_owned(),
                    );
                    item.terminal = Some(setup);
                    items.push(item);
                }
            }
            AnyRecord::MachineSample(r) if !r.deleted => {
                let name = fleet.name(r.owner);
                if let Some(limits) = fleet.settings.limits.get(&r.owner) {
                    for reason in over_limits(r, limits) {
                        if matches!(reason.over, Over::Disk { .. }) {
                            items.push(fleet.item(
                                Kind::DiskLow,
                                r.owner,
                                r.owner,
                                reason.text(&name),
                                "Free disk space on it, or lower its disk limit".to_owned(),
                            ));
                        }
                    }
                }
                if let Some(tag) = &r.body.update_refused {
                    items.push(fleet.item(
                        Kind::UpdateRefused,
                        r.owner,
                        r.owner,
                        format!("{name} rolled back {tag}: it did not become healthy"),
                        format!("Fix it in a higher tag; {name} skips {tag} until one ships"),
                    ));
                }
            }
            _ => {}
        }
    }

    for (key, overwritten) in &fleet.settings.overwritten {
        let machine = overwritten.lost_stamp.by;
        let setting = match key {
            SettingKey::Machine(id) => format!("the machine {}", fleet.name(*id)),
            SettingKey::Priority => "the coordinator order".to_owned(),
            SettingKey::Limits(id) => format!("the limits of {}", fleet.name(*id)),
            SettingKey::Project(id) => {
                let named = fleet.settings.projects.iter().find(|p| p.id == *id);
                let held = [&overwritten.kept, &overwritten.lost]
                    .into_iter()
                    .flatten()
                    .find_map(|value| match value {
                        SettingValue::Project(p) => Some(p),
                        _ => None,
                    });
                match named.or(held) {
                    Some(project) => format!("the project {}", project.name),
                    None => "a project".to_owned(),
                }
            }
        };
        items.push(fleet.item(
            Kind::SettingOverwritten,
            key,
            machine,
            format!(
                "Two sides of a split both edited {setting}: kept {}; replaced {}, the edit \
                 made on {}",
                value_text(fleet, overwritten.kept.as_ref()),
                value_text(fleet, overwritten.lost.as_ref()),
                fleet.name(machine)
            ),
            "Check it holds what you want, then dismiss this".to_owned(),
        ));
    }

    items
}

/// One value of an overwritten setting as the item shows it, `removed` for none. A project's
/// repo has any password hidden.
fn value_text(fleet: &Fleet, value: Option<&SettingValue>) -> String {
    match value {
        None => "removed".to_owned(),
        Some(SettingValue::Machine(machine)) => format!("{} ({})", machine.name, machine.address),
        Some(SettingValue::Priority(order)) => order
            .iter()
            .map(|id| fleet.name(*id))
            .collect::<Vec<_>>()
            .join(", "),
        Some(SettingValue::Project(project)) => {
            format!("{} ({})", project.name, redact(&project.repo))
        }
        Some(SettingValue::Limits(limits)) => format!(
            "at most {} sessions, load per core under {}, {:.1} GB free disk",
            limits.max_sessions,
            limits.max_load_per_core,
            limits.min_free_disk as f64 / 1e9
        ),
    }
}

/// The attention list of one machine: the fleet's items and those of each [`Source`].
pub struct Attention {
    me: MachineId,
    store: Arc<Mutex<Store>>,
    peers: Peers,
    coordinator: Arc<Coordinator>,
    sources: Vec<Arc<dyn Source>>,
    /// The last read of the list and when it was made. A poll holds this for the length of its
    /// read, so polls that arrive together wait for one read instead of each making their own.
    kept: tokio::sync::Mutex<Option<(Instant, Vec<Item>)>>,
}

impl Attention {
    pub fn new(
        me: MachineId,
        store: Arc<Mutex<Store>>,
        peers: Peers,
        coordinator: Arc<Coordinator>,
    ) -> Self {
        Self {
            me,
            store,
            peers,
            coordinator,
            sources: Vec::new(),
            kept: tokio::sync::Mutex::new(None),
        }
    }

    /// Adds `source`'s items to the list.
    pub fn with_source(mut self, source: Arc<dyn Source>) -> Self {
        self.sources.push(source);
        self
    }

    /// Every item that stands, ordered by kind and then id, from a read at most [`KEPT_FOR`]
    /// old. A read that fails is not kept.
    pub async fn items(&self) -> Result<Vec<Item>, StoreError> {
        let mut kept = self.kept.lock().await;
        if let Some((at, items)) = kept.as_ref()
            && at.elapsed() < KEPT_FOR
        {
            return Ok(items.clone());
        }
        let items = self.read().await?;
        *kept = Some((Instant::now(), items.clone()));
        Ok(items)
    }

    /// Reads the list now. Records are read as [`Peers::state`] shows them, so an offline
    /// machine's sessions and terminals are unreachable and raise nothing but its "machine
    /// offline" item.
    async fn read(&self) -> Result<Vec<Item>, StoreError> {
        let online = self.peers.online();
        let records = self.peers.state().await?;
        let settings = on_store(&self.store, |store| store.settings()).await?;
        let fleet = Fleet {
            me: self.me,
            records,
            settings,
            online,
        };
        let mut items = fleet_items(&fleet);
        for source in &self.sources {
            items.extend(source.items(&fleet));
        }
        items.sort_by(|a, b| (a.kind, &a.id).cmp(&(b.kind, &b.id)));
        Ok(items)
    }

    /// `GET /api/attention` and `POST /api/attention/{id}/dismiss`. The caller guards them as
    /// it guards the rest of the API.
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/api/attention", get(list))
            .route("/api/attention/{id}/dismiss", post(dismiss))
            .with_state(self)
    }
}

async fn list(State(attention): State<Arc<Attention>>) -> Response {
    match attention.items().await {
        Ok(items) => Json(serde_json::json!({ "items": items })).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn dismiss(State(attention): State<Arc<Attention>>, Path(id): Path<String>) -> Response {
    let missing = || error(StatusCode::NOT_FOUND, format!("no attention item {id:?}"));
    // A "setting overwritten" item stands while the settings hold its key, so the settings
    // alone say whether it does. Only an id of another kind is looked up in the list.
    let key = id
        .strip_prefix("setting_overwritten:")
        .and_then(|key| key.parse::<SettingKey>().ok());
    let Some(setting) = key else {
        return match attention.items().await {
            Ok(items) if items.iter().any(|item| item.id == id) => error(
                StatusCode::CONFLICT,
                "only a \"setting overwritten\" item can be dismissed; this one clears on its own"
                    .to_owned(),
            ),
            Ok(_) => missing(),
            Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
    };
    match on_store(&attention.store, |store| store.settings()).await {
        Ok(settings) if settings.overwritten.contains_key(&setting) => {}
        Ok(_) => return missing(),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
    match attention.coordinator.edit(Edit::Dismiss { setting }).await {
        Ok(()) => {
            // The read kept from before the edit still holds the item.
            *attention.kept.lock().await = None;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            let status = match e {
                CoordinatorError::NotLinked(_)
                | CoordinatorError::Older { .. }
                | CoordinatorError::Unsettled { .. } => StatusCode::SERVICE_UNAVAILABLE,
                CoordinatorError::Unwritable(_) | CoordinatorError::Store(_) => {
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            };
            error(status, e.to_string())
        }
    }
}

/// `{"error": message}` with any URL password in it hidden, logged when the fault is ours.
fn error(status: StatusCode, message: String) -> Response {
    let message = redact(&message);
    if status.is_server_error() {
        tracing::warn!("attention: {message}");
    }
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use foundry_proto::ids::{ProjectId, SessionId, WorkspaceId};
    use foundry_proto::messages::{PROTOCOL, PeerMessage};
    use foundry_proto::record::{MachineSample, Record, Session, Workspace};
    use foundry_proto::settings::{Limits, Machine, Overwritten, Project, Stamp};
    use futures::future::BoxFuture;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::sync::{Conn, Dialer};

    const GB: u64 = 1_000_000_000;

    /// This machine and one peer, both named and both heard from, holding nothing yet.
    struct World {
        me: MachineId,
        peer: MachineId,
        records: Vec<AnyRecord>,
        settings: SharedSettings,
        online: BTreeSet<MachineId>,
        version: u64,
    }

    impl World {
        fn new() -> Self {
            let (me, peer) = (MachineId::random(), MachineId::random());
            let machine = |id, name: &str| Machine {
                id,
                name: name.into(),
                address: "100.64.0.1".into(),
            };
            Self {
                me,
                peer,
                records: Vec::new(),
                settings: SharedSettings {
                    machines: vec![machine(me, "this-mac"), machine(peer, "trilogy-mac")],
                    ..SharedSettings::default()
                },
                online: [me, peer].into(),
                version: 0,
            }
        }

        fn record<T>(&mut self, owner: MachineId, deleted: bool, body: T) -> Record<T> {
            self.version += 1;
            Record {
                owner,
                version: self.version,
                deleted,
                body,
            }
        }

        /// Puts `record` in place of any held record of the same kind and id.
        fn put(&mut self, record: AnyRecord) {
            let same = |a: &AnyRecord, b: &AnyRecord| match (a, b) {
                (AnyRecord::Workspace(a), AnyRecord::Workspace(b)) => a.body.id == b.body.id,
                (AnyRecord::Terminal(a), AnyRecord::Terminal(b)) => a.body.id == b.body.id,
                (AnyRecord::Session(a), AnyRecord::Session(b)) => a.body.id == b.body.id,
                (AnyRecord::MachineSample(a), AnyRecord::MachineSample(b)) => a.owner == b.owner,
                _ => false,
            };
            self.records.retain(|held| !same(held, &record));
            self.records.push(record);
        }

        fn workspace(&mut self, owner: MachineId, setup: Option<TerminalId>) -> WorkspaceId {
            let id = WorkspaceId::random();
            self.put_workspace(owner, id, setup, false);
            id
        }

        fn put_workspace(
            &mut self,
            owner: MachineId,
            id: WorkspaceId,
            setup: Option<TerminalId>,
            deleted: bool,
        ) {
            let body = Workspace {
                id,
                project: ProjectId::random(),
                name: "fix login".into(),
                branch: "fix-login".into(),
                base: "main".into(),
                setup,
            };
            let record = self.record(owner, deleted, body);
            self.put(AnyRecord::Workspace(record));
        }

        fn terminal(
            &mut self,
            owner: MachineId,
            id: TerminalId,
            workspace: WorkspaceId,
            exited: Option<Option<i32>>,
            deleted: bool,
        ) {
            let body = Terminal {
                id,
                workspace,
                state: if exited.is_some() {
                    TerminalState::Exited
                } else {
                    TerminalState::Running
                },
                exit_code: exited.flatten(),
            };
            let record = self.record(owner, deleted, body);
            self.put(AnyRecord::Terminal(record));
        }

        /// A session of `owner` in `state`, in a workspace of its own with a running terminal.
        fn session(&mut self, owner: MachineId, state: SessionState) -> Session {
            let workspace = self.workspace(owner, None);
            let terminal = TerminalId::random();
            self.terminal(owner, terminal, workspace, None, false);
            let body = Session {
                id: SessionId::random(),
                workspace,
                terminal,
                model: "opus".into(),
                state,
            };
            self.put_session(owner, body.clone(), false);
            body
        }

        fn put_session(&mut self, owner: MachineId, body: Session, deleted: bool) {
            let record = self.record(owner, deleted, body);
            self.put(AnyRecord::Session(record));
        }

        fn sample(&mut self, owner: MachineId, free_disk: u64, refused: Option<&str>) {
            let body = MachineSample {
                load_per_core: 0.1,
                free_memory: 8 * GB,
                free_disk,
                sessions: 0,
                update_refused: refused.map(str::to_owned),
            };
            let record = self.record(owner, false, body);
            self.put(AnyRecord::MachineSample(record));
        }

        fn items(&self) -> Vec<Item> {
            fleet_items(&Fleet {
                me: self.me,
                records: self.records.clone(),
                settings: self.settings.clone(),
                online: self.online.clone(),
            })
        }

        fn of(&self, kind: Kind) -> Vec<Item> {
            self.items()
                .into_iter()
                .filter(|i| i.kind == kind)
                .collect()
        }
    }

    // --- machine offline ---

    #[test]
    fn a_machine_not_heard_from_raises_machine_offline() {
        let mut world = World::new();
        world.online.remove(&world.peer);
        let items = world.of(Kind::MachineOffline);
        assert_eq!(items.len(), 1, "{items:?}");
        let item = &items[0];
        assert_eq!(item.id, format!("machine_offline:{}", world.peer));
        assert_eq!(item.machine, world.peer);
        assert_eq!(item.machine_name, "trilogy-mac");
        assert!(item.what.contains("trilogy-mac"), "{}", item.what);
        assert!(item.what.contains("15 seconds"), "{}", item.what);
        assert!(!item.todo.is_empty());
    }

    #[test]
    fn a_machine_heard_from_again_clears_machine_offline() {
        let mut world = World::new();
        world.online.remove(&world.peer);
        assert_eq!(world.of(Kind::MachineOffline).len(), 1);
        world.online.insert(world.peer);
        assert!(world.of(Kind::MachineOffline).is_empty());
    }

    // --- sessions ---

    #[test]
    fn a_stuck_session_raises_session_stuck() {
        let mut world = World::new();
        let session = world.session(world.peer, SessionState::Stuck);
        let items = world.of(Kind::SessionStuck);
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].id, format!("session_stuck:{}", session.id));
        assert_eq!(items[0].machine, world.peer);
        assert_eq!(items[0].terminal, Some(session.terminal));
        assert!(items[0].what.contains("10 minutes"), "{}", items[0].what);
    }

    #[test]
    fn output_or_an_event_or_the_end_of_the_session_clears_session_stuck() {
        for (state, deleted) in [
            (SessionState::Working, false),
            (SessionState::Idle, false),
            (SessionState::Finished, false),
            (SessionState::Stuck, true),
        ] {
            let mut world = World::new();
            let mut session = world.session(world.me, SessionState::Stuck);
            assert_eq!(world.of(Kind::SessionStuck).len(), 1);
            session.state = state;
            world.put_session(world.me, session, deleted);
            assert!(world.of(Kind::SessionStuck).is_empty(), "{state:?}");
        }
    }

    #[test]
    fn a_session_waiting_for_you_raises_session_waiting() {
        let mut world = World::new();
        let session = world.session(world.me, SessionState::WaitingForYou);
        let items = world.of(Kind::SessionWaiting);
        assert_eq!(items.len(), 1, "{items:?}");
        let item = &items[0];
        assert_eq!(item.id, format!("session_waiting:{}", session.id));
        assert_eq!(item.machine_name, "this-mac");
        assert_eq!(item.terminal, Some(session.terminal));
        assert_eq!(
            item.what,
            "A session in fix login on this-mac is waiting for you"
        );
    }

    #[test]
    fn an_answer_clears_session_waiting() {
        let mut world = World::new();
        let mut session = world.session(world.me, SessionState::WaitingForYou);
        assert_eq!(world.of(Kind::SessionWaiting).len(), 1);
        session.state = SessionState::Working;
        world.put_session(world.me, session, false);
        assert!(world.of(Kind::SessionWaiting).is_empty());
    }

    #[test]
    fn a_failed_session_raises_session_failed_with_its_exit_code() {
        let mut world = World::new();
        let peer = world.peer;
        let session = world.session(peer, SessionState::Failed);
        world.terminal(
            peer,
            session.terminal,
            session.workspace,
            Some(Some(2)),
            false,
        );
        let items = world.of(Kind::SessionFailed);
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].id, format!("session_failed:{}", session.id));
        assert_eq!(items[0].terminal, Some(session.terminal));
        assert!(
            items[0].what.ends_with("failed (exit code 2)"),
            "{}",
            items[0].what
        );
    }

    #[test]
    fn closing_its_terminal_clears_session_failed() {
        let mut world = World::new();
        let peer = world.peer;
        let session = world.session(peer, SessionState::Failed);
        assert_eq!(world.of(Kind::SessionFailed).len(), 1);
        world.terminal(
            peer,
            session.terminal,
            session.workspace,
            Some(Some(2)),
            true,
        );
        assert!(world.of(Kind::SessionFailed).is_empty());
    }

    #[test]
    fn a_session_in_any_other_state_raises_nothing() {
        let mut world = World::new();
        for state in [
            SessionState::Starting,
            SessionState::Working,
            SessionState::Idle,
            SessionState::Finished,
            SessionState::Unreachable,
        ] {
            world.session(world.me, state);
        }
        assert!(world.items().is_empty(), "{:?}", world.items());
    }

    // --- setup failed ---

    /// A workspace of the peer whose setup terminal exited with `code`.
    fn setup_exited(world: &mut World, code: Option<i32>) -> (WorkspaceId, TerminalId) {
        let terminal = TerminalId::random();
        let workspace = world.workspace(world.peer, Some(terminal));
        world.terminal(world.peer, terminal, workspace, Some(code), false);
        (workspace, terminal)
    }

    #[test]
    fn a_setup_terminal_that_exits_non_zero_raises_setup_failed() {
        for code in [Some(3), None] {
            let mut world = World::new();
            let (workspace, terminal) = setup_exited(&mut world, code);
            let items = world.of(Kind::SetupFailed);
            assert_eq!(items.len(), 1, "{code:?}: {items:?}");
            assert_eq!(items[0].id, format!("setup_failed:{workspace}"));
            assert_eq!(items[0].machine, world.peer);
            assert_eq!(items[0].terminal, Some(terminal));
            assert!(items[0].what.contains("fix login"), "{}", items[0].what);
        }
    }

    #[test]
    fn a_setup_that_runs_or_passes_raises_nothing() {
        let mut world = World::new();
        setup_exited(&mut world, Some(0));
        let running = TerminalId::random();
        let workspace = world.workspace(world.peer, Some(running));
        world.terminal(world.peer, running, workspace, None, false);
        assert!(world.of(Kind::SetupFailed).is_empty());
    }

    #[test]
    fn archiving_the_workspace_clears_setup_failed() {
        let mut world = World::new();
        let (workspace, terminal) = setup_exited(&mut world, Some(3));
        assert_eq!(world.of(Kind::SetupFailed).len(), 1);
        world.put_workspace(world.peer, workspace, Some(terminal), true);
        assert!(world.of(Kind::SetupFailed).is_empty());
    }

    #[test]
    fn closing_the_setup_terminal_clears_setup_failed() {
        let mut world = World::new();
        let (workspace, terminal) = setup_exited(&mut world, Some(3));
        assert_eq!(world.of(Kind::SetupFailed).len(), 1);
        world.terminal(world.peer, terminal, workspace, Some(Some(3)), true);
        assert!(world.of(Kind::SetupFailed).is_empty());
    }

    // --- disk low ---

    fn limit_disk(world: &mut World, machine: MachineId, min_free_disk: u64) {
        world.settings.limits.insert(
            machine,
            Limits {
                max_sessions: 4,
                max_load_per_core: 1.0,
                min_free_disk,
            },
        );
    }

    #[test]
    fn free_disk_under_the_limit_raises_disk_low() {
        let mut world = World::new();
        let peer = world.peer;
        limit_disk(&mut world, peer, 10 * GB);
        world.sample(world.peer, 8 * GB, None);
        let items = world.of(Kind::DiskLow);
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].id, format!("disk_low:{}", world.peer));
        assert_eq!(
            items[0].what,
            "trilogy-mac: free disk 8.0 GB under limit 10.0 GB"
        );
    }

    #[test]
    fn free_disk_back_over_the_limit_clears_disk_low() {
        let mut world = World::new();
        let peer = world.peer;
        limit_disk(&mut world, peer, 10 * GB);
        world.sample(world.peer, 8 * GB, None);
        assert_eq!(world.of(Kind::DiskLow).len(), 1);
        world.sample(world.peer, 12 * GB, None);
        assert!(world.of(Kind::DiskLow).is_empty());
    }

    #[test]
    fn a_machine_over_its_session_or_load_limit_is_not_low_on_disk() {
        let mut world = World::new();
        let peer = world.peer;
        limit_disk(&mut world, peer, 10 * GB);
        let body = MachineSample {
            load_per_core: 3.0,
            free_memory: 8 * GB,
            free_disk: 100 * GB,
            sessions: 9,
            update_refused: None,
        };
        let record = world.record(peer, false, body);
        world.put(AnyRecord::MachineSample(record));
        assert!(world.of(Kind::DiskLow).is_empty(), "{:?}", world.items());
    }

    #[test]
    fn a_machine_with_no_limits_is_never_low_on_disk() {
        let mut world = World::new();
        world.sample(world.peer, 1, None);
        assert!(world.of(Kind::DiskLow).is_empty());
    }

    // --- setting overwritten ---

    fn overwrite_project(world: &mut World) -> (SettingKey, Project) {
        let project = Project {
            id: ProjectId::random(),
            name: "foundry".into(),
            repo: "https://tom:hunter2@github.com/o/foundry.git".into(),
        };
        let key = SettingKey::Project(project.id);
        world.settings.overwritten.insert(
            key.clone(),
            Overwritten {
                kept: Some(SettingValue::Project(project.clone())),
                lost: None,
                lost_stamp: Stamp {
                    seq: 3,
                    by: world.peer,
                },
            },
        );
        (key, project)
    }

    #[test]
    fn an_overwritten_setting_raises_setting_overwritten_with_both_values_and_no_password() {
        let mut world = World::new();
        let (key, project) = overwrite_project(&mut world);
        let lost = Project {
            name: "foundry-old".into(),
            repo: "https://github.com/o/old.git".into(),
            ..project
        };
        world.settings.overwritten.get_mut(&key).unwrap().lost = Some(SettingValue::Project(lost));
        let items = world.of(Kind::SettingOverwritten);
        assert_eq!(items.len(), 1, "{items:?}");
        let item = &items[0];
        assert_eq!(item.id, format!("setting_overwritten:{key}"));
        assert_eq!(item.machine, world.peer);
        assert!(item.what.contains("the project foundry"), "{}", item.what);
        assert!(
            item.what
                .contains("kept foundry (https://***@github.com/o/foundry.git)"),
            "{}",
            item.what
        );
        assert!(
            item.what
                .contains("replaced foundry-old (https://github.com/o/old.git)"),
            "{}",
            item.what
        );
        let text = serde_json::to_string(item).unwrap();
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn an_overwritten_value_that_was_removed_or_is_limits_or_an_order_reads_plainly() {
        let mut world = World::new();
        let (me, peer) = (world.me, world.peer);
        let limits = Limits {
            max_sessions: 4,
            max_load_per_core: 1.5,
            min_free_disk: 10 * GB,
        };
        world.settings.overwritten.insert(
            SettingKey::Limits(peer),
            Overwritten {
                kept: Some(SettingValue::Limits(limits)),
                lost: None,
                lost_stamp: Stamp { seq: 2, by: me },
            },
        );
        world.settings.overwritten.insert(
            SettingKey::Priority,
            Overwritten {
                kept: Some(SettingValue::Priority(vec![me, peer])),
                lost: Some(SettingValue::Priority(vec![peer, me])),
                lost_stamp: Stamp { seq: 2, by: peer },
            },
        );
        let items = world.of(Kind::SettingOverwritten);
        let limits = items.iter().find(|i| i.id.contains("limits")).unwrap();
        assert!(
            limits.what.contains(
                "kept at most 4 sessions, load per core under 1.5, 10.0 GB free disk; \
                 replaced removed"
            ),
            "{}",
            limits.what
        );
        let order = items.iter().find(|i| i.id.ends_with("priority")).unwrap();
        assert!(
            order
                .what
                .contains("kept this-mac, trilogy-mac; replaced trilogy-mac, this-mac"),
            "{}",
            order.what
        );
    }

    #[test]
    fn a_dismissed_setting_clears_setting_overwritten() {
        let mut world = World::new();
        let (key, _) = overwrite_project(&mut world);
        assert_eq!(world.of(Kind::SettingOverwritten).len(), 1);
        world.settings.overwritten.remove(&key);
        assert!(world.of(Kind::SettingOverwritten).is_empty());
    }

    // --- update refused ---

    #[test]
    fn a_refused_tag_raises_update_refused_for_every_machine() {
        let mut world = World::new();
        world.sample(world.me, 100 * GB, Some("v0.2.0"));
        world.sample(world.peer, 100 * GB, Some("v0.3.0"));
        let items = world.of(Kind::UpdateRefused);
        assert_eq!(items.len(), 2, "{items:?}");
        let peer = items.iter().find(|i| i.machine == world.peer).unwrap();
        assert_eq!(peer.id, format!("update_refused:{}", world.peer));
        assert!(peer.what.contains("v0.3.0"), "{}", peer.what);
    }

    #[test]
    fn a_sample_without_a_refused_tag_clears_update_refused() {
        let mut world = World::new();
        world.sample(world.peer, 100 * GB, Some("v0.2.0"));
        assert_eq!(world.of(Kind::UpdateRefused).len(), 1);
        world.sample(world.peer, 100 * GB, None);
        assert!(world.of(Kind::UpdateRefused).is_empty());
    }

    // --- the list and its routes ---

    struct NoDial;

    impl Dialer for NoDial {
        fn dial(&self, _: &Machine) -> BoxFuture<'static, std::io::Result<Conn>> {
            Box::pin(async { Err(std::io::Error::other("no peers in these tests")) })
        }
    }

    /// A source that raises one item, as a later module would, and counts how often it is
    /// read: once for every read of the records.
    #[derive(Default)]
    struct Fixed(AtomicUsize);

    impl Source for Fixed {
        fn items(&self, fleet: &Fleet) -> Vec<Item> {
            self.0.fetch_add(1, Ordering::SeqCst);
            vec![fleet.item(
                Kind::GatewayUnhealthy,
                fleet.me,
                fleet.me,
                "from a source".into(),
                "nothing".into(),
            )]
        }
    }

    struct Served {
        _data: tempfile::TempDir,
        me: MachineId,
        store: Arc<Mutex<Store>>,
        peers: Peers,
        attention: Arc<Attention>,
        source: Arc<Fixed>,
        addr: std::net::SocketAddr,
        _server: tokio::task::JoinHandle<()>,
    }

    impl Served {
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
            let source = Arc::new(Fixed::default());
            let attention = Arc::new(
                Attention::new(me, store.clone(), peers.clone(), coordinator)
                    .with_source(source.clone()),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let router = attention.clone().router();
            let server = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            Self {
                _data: data,
                me,
                store,
                peers,
                attention,
                source,
                addr,
                _server: server,
            }
        }

        /// `(status, body)` of `method path`.
        async fn call(&self, method: &str, path: &str) -> (u16, String) {
            let mut stream = tokio::net::TcpStream::connect(self.addr).await.unwrap();
            let request = format!(
                "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                self.addr
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

        /// `peer` says hello and sends nothing, so it is heard from for [`OFFLINE_AFTER`], then
        /// its link drops.
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

        /// Stores every record `world` holds, as their owners sent them.
        fn hold(&self, world: &World) {
            let mut store = self.store.lock().unwrap();
            for record in &world.records {
                store.apply(record, record.owner()).unwrap();
            }
        }

        async fn kinds(&self) -> Vec<(String, String)> {
            let (status, body) = self.call("GET", "/api/attention").await;
            assert_eq!(status, 200, "{body}");
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            json["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| {
                    (
                        i["kind"].as_str().unwrap().to_owned(),
                        i["machine"].as_str().unwrap().to_owned(),
                    )
                })
                .collect()
        }

        /// How many times the records were read to make the list.
        fn reads(&self) -> usize {
            self.source.0.load(Ordering::SeqCst)
        }

        fn overwritten(&self) -> BTreeMap<SettingKey, Overwritten> {
            self.store.lock().unwrap().settings().unwrap().overwritten
        }
    }

    /// Settings with one overwritten project, whose repo holds a password.
    fn with_overwritten(me: MachineId, settings: &mut SharedSettings) {
        let project = Project {
            id: ProjectId::random(),
            name: "secret-name".into(),
            repo: "https://tom:hunter2@github.com/o/foundry.git".into(),
        };
        settings.overwritten.insert(
            SettingKey::Project(project.id),
            Overwritten {
                kept: Some(SettingValue::Project(project.clone())),
                lost: Some(SettingValue::Project(Project {
                    name: "other-name".into(),
                    ..project
                })),
                lost_stamp: Stamp { seq: 1, by: me },
            },
        );
    }

    #[tokio::test]
    async fn the_list_holds_the_fleets_items_and_each_sources() {
        let served = Served::new(with_overwritten).await;
        let (status, body) = served.call("GET", "/api/attention").await;
        assert_eq!(status, 200, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let items = json["items"].as_array().unwrap();
        let kinds: Vec<&str> = items.iter().map(|i| i["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["gateway_unhealthy", "setting_overwritten"]);
        for item in items {
            for field in ["id", "what", "todo", "machine", "machine_name"] {
                assert!(item[field].is_string(), "{field}: {item}");
            }
            assert_eq!(item["machine"], served.me.to_string());
        }
        assert!(!body.contains("hunter2"), "{body}");
    }

    #[tokio::test(start_paused = true)]
    async fn reads_within_the_time_a_read_is_kept_share_one_read_of_the_records() {
        let mut world = World::new();
        let (peer, machines) = (world.peer, world.settings.machines.clone());
        let served = Served::new(|_, settings| settings.machines = machines).await;
        let refused = |items: Vec<Item>| items.iter().any(|i| i.kind == Kind::UpdateRefused);

        // Two tabs polling at once, then a third poll just after.
        let (first, second) = tokio::join!(served.attention.items(), served.attention.items());
        assert_eq!(first.unwrap(), second.unwrap());
        world.sample(peer, 100 * GB, Some("v0.2.0"));
        served.hold(&world);
        tokio::time::advance(KEPT_FOR - Duration::from_millis(1)).await;
        assert!(!refused(served.attention.items().await.unwrap()));
        assert_eq!(served.reads(), 1);

        // Once the read is no longer kept, the next poll reads again and sees what changed.
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(refused(served.attention.items().await.unwrap()));
        assert_eq!(served.reads(), 2);
    }

    /// A world whose peer has a session waiting for you, a failed setup and a refused tag.
    fn busy_peer(world: &mut World) {
        let peer = world.peer;
        world.session(peer, SessionState::WaitingForYou);
        let setup = TerminalId::random();
        let workspace = world.workspace(peer, Some(setup));
        world.terminal(peer, setup, workspace, Some(Some(1)), false);
        world.sample(peer, 100 * GB, Some("v0.2.0"));
    }

    #[tokio::test]
    async fn a_peers_records_raise_its_items_on_this_machine() {
        let mut world = World::new();
        busy_peer(&mut world);
        let (peer, machines) = (world.peer, world.settings.machines.clone());
        let served = Served::new(|_, settings| settings.machines = machines).await;
        served.hold(&world);
        served.hear(peer).await;

        let peer = peer.to_string();
        let kinds = served.kinds().await;
        for kind in ["session_waiting", "setup_failed", "update_refused"] {
            assert!(
                kinds.contains(&(kind.to_owned(), peer.clone())),
                "{kind}: {kinds:?}"
            );
        }
        assert!(
            !kinds.contains(&("machine_offline".to_owned(), peer)),
            "{kinds:?}"
        );
    }

    #[tokio::test]
    async fn an_offline_machines_sessions_and_setup_raise_nothing_but_machine_offline() {
        let mut world = World::new();
        busy_peer(&mut world);
        let (peer, machines) = (world.peer, world.settings.machines.clone());
        let served = Served::new(|_, settings| settings.machines = machines).await;
        served.hold(&world);

        let peer = peer.to_string();
        let mut kinds: Vec<&str> = Vec::new();
        let held = served.kinds().await;
        for (kind, machine) in &held {
            if *machine == peer {
                kinds.push(kind);
            }
        }
        kinds.sort_unstable();
        // The refused tag is the sample's, which an offline machine keeps.
        assert_eq!(kinds, ["machine_offline", "update_refused"], "{held:?}");
    }

    #[tokio::test]
    async fn dismissing_a_setting_overwritten_item_removes_it_from_the_settings() {
        let served = Served::new(with_overwritten).await;
        let key = served.overwritten().into_keys().next().unwrap();
        // The list was read a moment ago, as the UI that shows the item has.
        let items = served.attention.items().await.unwrap();
        assert!(
            items.iter().any(|i| i.kind == Kind::SettingOverwritten),
            "{items:?}"
        );
        let (status, body) = served
            .call(
                "POST",
                &format!("/api/attention/setting_overwritten:{key}/dismiss"),
            )
            .await;
        assert_eq!(status, 204, "{body}");
        assert!(served.overwritten().is_empty());
        // The read kept from before the dismissal is not shown again.
        let items = served.attention.items().await.unwrap();
        assert!(
            items.iter().all(|i| i.kind != Kind::SettingOverwritten),
            "{items:?}"
        );
    }

    #[tokio::test]
    async fn dismissing_a_setting_overwritten_item_reads_the_settings_and_no_records() {
        let served = Served::new(with_overwritten).await;
        let key = served.overwritten().into_keys().next().unwrap();
        let (status, body) = served
            .call(
                "POST",
                &format!("/api/attention/setting_overwritten:{key}/dismiss"),
            )
            .await;
        assert_eq!(status, 204, "{body}");
        assert_eq!(served.reads(), 0);
    }

    #[tokio::test]
    async fn dismissing_any_other_item_is_a_conflict_and_changes_nothing() {
        let served = Served::new(with_overwritten).await;
        let id = format!("gateway_unhealthy:{}", served.me);
        let (status, body) = served
            .call("POST", &format!("/api/attention/{id}/dismiss"))
            .await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(served.overwritten().len(), 1);
    }

    #[tokio::test]
    async fn dismissing_an_item_that_does_not_stand_is_not_found() {
        let served = Served::new(|_, _| {}).await;
        let key = SettingKey::Project(ProjectId::random());
        for id in [format!("setting_overwritten:{key}"), "nonsense".to_owned()] {
            let (status, body) = served
                .call("POST", &format!("/api/attention/{id}/dismiss"))
                .await;
            assert_eq!(status, 404, "{id}: {body}");
        }
    }

    #[tokio::test]
    async fn a_dismissal_the_coordinator_cannot_send_answers_its_error_without_the_value() {
        let peer = MachineId::random();
        let served = Served::new(|me, settings| {
            with_overwritten(me, settings);
            settings.machines = vec![Machine {
                id: peer,
                name: "trilogy-mac".into(),
                address: "100.64.0.2".into(),
            }];
            settings.priority = vec![peer, me];
        })
        .await;
        // The peer is heard from, so it acts, then its link drops: the dismissal has nowhere
        // to go.
        served.hear(peer).await;

        let key = served.overwritten().into_keys().next().unwrap();
        let (status, body) = served
            .call(
                "POST",
                &format!("/api/attention/setting_overwritten:{key}/dismiss"),
            )
            .await;
        assert_eq!(status, 503, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let error = json["error"].as_str().unwrap();
        assert!(error.contains(&peer.to_string()), "{error}");
        for value in ["hunter2", "github.com", "secret-name", "other-name"] {
            assert!(!body.contains(value), "{value} in {body}");
        }
        assert_eq!(served.overwritten().len(), 1);
    }
}
