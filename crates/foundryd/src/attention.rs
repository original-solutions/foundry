//! Raises and clears attention items: what needs a person, with what happened and what to do.
//!
//! An item is not stored. It is read from what holds now, so it is raised as soon as its rule
//! holds and cleared as soon as it no longer does. The UI polls the list, so a read loads only
//! the records written since the read before it, never the whole store again. The factory
//! statuses are the exception: each read takes in full the ones held of each project whose
//! factory is on, one row per project and writing machine:
//!
//! | Item | Raised while | Read from |
//! |---|---|---|
//! | Machine offline | a machine in the settings is not heard from for [`OFFLINE_AFTER`] | the peer links |
//! | Session stuck | a session is stuck | its record |
//! | Session waiting for you | a session waits for you | its record |
//! | Session failed | a session failed and its terminal is not closed | its record and its terminal's |
//! | Setup failed | a live workspace's setup terminal exited other than 0 and is not closed | the workspace's record and its terminal's |
//! | Disk low | a machine's newest sample has less free disk than its limit | the sample and the limits |
//! | Placement waiting | a request of this machine has waited over 5 minutes for a machine | [`crate::placement`] |
//! | Setting overwritten | the shared settings hold an overwritten setting | the settings |
//! | Update refused | a machine's newest sample names a refused tag | the sample |
//! | Gateway unhealthy | this machine's read of the gateway fails | [`crate::gateway`] |
//! | Run stuck | the believed status lists a stuck run | the factory statuses, and the run's session and its terminal |
//! | Run needs info | the believed status lists a run that asked questions | the factory statuses, and the run's session and its terminal |
//! | Factory idle | the believed status gives a reason, or for [`IDLE_AFTER`] since it saw the factory on this machine has neither held the lease nor heard from its holder | the factory statuses and [`Quiet`], given by [`Attention::with_factory`] |
//! | Foreign claim | the believed status lists an issue claimed by something other than Foundry | the factory statuses |
//! | Gate infra | the believed status lists a run whose gate could not run at all | the factory statuses |
//!
//! Every row but the gateway's and placement's comes from the synced records, the shared
//! settings and the factory statuses, so each machine shows the whole fleet's. The one
//! exception within them is "factory idle" for want of word from a holder: [`Quiet`] counts
//! what this machine has heard, so one machine can show it while another does not. Records are read as the rest of the UI sees them: an offline
//! machine's sessions and terminals are unreachable, so they raise nothing while its "machine
//! offline" item stands. Another module adds rows through a [`Source`].
//!
//! The factory's rows are read only for a project whose factory is on, from the status
//! [`believed`] picks among the online writers' (P2, "Attention items in P2"). The factory
//! work of such a project raises none of P1's rows for it (G13), since the run's own row
//! covers it:
//!
//! - a session named in a live run by a status of the project at the highest lease generation
//!   any of its statuses gives, or that this machine's [`Marks`] say is one of its factory
//!   sessions of the project, raises no session stuck, waiting or failed item. So a holder
//!   that went offline names its runs' sessions until a newer holder takes over, and a writer
//!   a newer holder superseded names none, even while it is believed. Another
//!   session in a factory workspace, such as one a person starts in a stuck run's workspace,
//!   still raises its items;
//! - a workspace such a status names, that [`Marks`] say is one of this machine's factory
//!   workspaces of the project, or that holds such a factory session, raises no setup failed
//!   item.
//!
//! A project whose factory is off has no factory work: its sessions and workspaces raise P1's
//! items again, as no factory row covers them.
//!
//! - `GET /api/attention`: `{"items": [...]}`, each an [`Item`].
//! - `POST /api/attention/{id}/dismiss`: dismisses a "setting overwritten" item through the
//!   coordinator, `204` once the dismissal is numbered. Any other item is `409`; an id that
//!   names no item is `404`. A dismissal the acting coordinator refuses is `502`, and one it
//!   does not answer in time `504`; a failed one answers with its error, which names no
//!   setting's value.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use foundry_proto::factory::{FactoryStatus, HeldUpKind, NotHolding, RunId};
use foundry_proto::ids::{MachineId, ProjectId, SessionId, TerminalId, WorkspaceId};
use foundry_proto::record::{AnyRecord, Session, Terminal};
use foundry_proto::settings::{
    Edit, Project, SettingKey, SettingValue, SharedSettings, Stamp, StampKey,
};
use foundry_proto::state::{SessionState, TerminalState};
use serde::Serialize;

use crate::api::edit_status;
use crate::clock::Clock;
use crate::coordinator::{self, Coordinator};
use crate::factory::status::{Freshness, believed};
use crate::machine::{Over, over_limits};
use crate::secrets::redact;
use crate::sessions::STUCK_AFTER;
use crate::store::{Store, StoreError};
use crate::sync::{OFFLINE_AFTER, Peers, as_seen, lock, on_store};

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
    PlacementWaiting,
    SettingOverwritten,
    UpdateRefused,
    RunStuck,
    RunNeedsInfo,
    FactoryIdle,
    ForeignClaim,
    GateInfra,
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
            Self::PlacementWaiting => "placement_waiting",
            Self::SettingOverwritten => "setting_overwritten",
            Self::UpdateRefused => "update_refused",
            Self::RunStuck => "run_stuck",
            Self::RunNeedsInfo => "run_needs_info",
            Self::FactoryIdle => "factory_idle",
            Self::ForeignClaim => "foreign_claim",
            Self::GateInfra => "gate_infra",
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
    /// The GitHub issue of the run it is about, when it is about a run.
    pub issue_url: Option<String>,
    /// The pull request the run reported, when it did.
    pub pr_url: Option<String>,
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
            issue_url: None,
            pr_url: None,
        }
    }
}

/// A module that raises items of its own, read on every list of items.
pub trait Source: Send + Sync {
    fn items(&self, fleet: &Fleet) -> Vec<Item>;
}

/// How long a project whose factory is on may go with this machine neither holding its lease
/// nor hearing from its holder before "factory idle" is raised: the cut-off's 10 minutes (G10).
pub const IDLE_AFTER: Duration = Duration::from_secs(10 * 60);

/// What this machine knows as factory work of its own: the mark `sessions` keeps beside a
/// factory session, and the one `workspaces` keeps beside a factory workspace. Asked only of
/// this machine's own records; a peer's are known from the statuses alone.
pub trait Marks: Send + Sync {
    /// The project `session` is one of this machine's factory sessions of, if it is one.
    fn factory_session(&self, session: SessionId) -> Option<ProjectId>;
    /// The project `workspace` is one of this machine's factory workspaces of, if it is one.
    fn factory_workspace(&self, workspace: WorkspaceId) -> Option<ProjectId>;
}

/// How long each project whose factory is on has gone with this machine neither holding its
/// lease nor hearing from its holder, as [`Freshness`] counts it, but never longer than since
/// its factory setting last changed: a factory just turned on is not idle for the time it was
/// off.
///
/// A change is read from the setting's stamp, which every edit of it renews, so a factory
/// turned off and on between two reads of the list still counts from the read that finds the
/// new stamp. A project whose setting is there at the first read counts from when this was
/// made, as [`Freshness`] counts a project never heard from; one whose setting first appears
/// at a later read counts from that read. So a factory turned on before this machine's first
/// read since it started is counted from the start, a factory setting that arrives later is
/// not idle for the time before it arrived, and any edit of a project's factory setting, a
/// new login too, starts its count again at the next read.
pub struct Quiet {
    clock: Arc<dyn Clock>,
    freshness: Arc<Freshness>,
    /// The clock's reading when this was made.
    made: Duration,
    /// Each project's factory setting as last read. `None` until the first read.
    seen: Mutex<Option<Counts>>,
}

/// Each project's factory setting as [`Quiet`] last read it: its stamp, and the clock's reading
/// its count starts from.
type Counts = BTreeMap<ProjectId, (Option<Stamp>, Duration)>;

impl Quiet {
    /// Reads `freshness`, which must count on `clock`.
    pub fn new(clock: Arc<dyn Clock>, freshness: Arc<Freshness>) -> Self {
        let made = clock.now();
        Self {
            clock,
            freshness,
            made,
            seen: Mutex::new(None),
        }
    }

    /// How long each of `on`, the projects whose factory is on in `settings`, has been quiet.
    fn of(
        &self,
        settings: &SharedSettings,
        on: &BTreeSet<ProjectId>,
    ) -> BTreeMap<ProjectId, Duration> {
        let now = self.clock.now();
        let mut seen = lock(&self.seen);
        // A setting there at the first read counts from when this was made; one that appears
        // later, from the read that first finds it.
        let first_seen = if seen.is_some() { now } else { self.made };
        let seen = seen.get_or_insert_with(Counts::new);
        seen.retain(|project, _| settings.project_factory.contains_key(project));
        for project in settings.project_factory.keys() {
            let key = StampKey::Setting(SettingKey::ProjectFactory(*project));
            let stamp = settings.stamps.get(&key).copied();
            let held = seen.entry(*project).or_insert((stamp, first_seen));
            if held.0 != stamp {
                *held = (stamp, now);
            }
        }
        on.iter()
            .map(|project| {
                let since = seen.get(project).map_or(first_seen, |(_, since)| *since);
                let quiet = now.saturating_sub(since);
                (*project, quiet.min(self.freshness.since_holding(*project)))
            })
            .collect()
    }
}

/// What the factory's rows read beside the [`Fleet`].
#[derive(Default)]
pub struct FactoryFleet<'a> {
    /// Every status held of each project whose factory is on, from every writer, online or
    /// not. A project left out has none.
    pub statuses: BTreeMap<ProjectId, Vec<(MachineId, FactoryStatus)>>,
    /// This machine's quiet, when it keeps one. Without it "factory idle" is raised only for a
    /// reason a status gives.
    pub quiet: Option<&'a Quiet>,
    /// What this machine marks as factory work, when it is told.
    pub marks: Option<&'a dyn Marks>,
}

/// A project whose factory is on, and the status believed of it with its writer.
struct OnProject<'a> {
    project: &'a Project,
    believed: Option<(MachineId, &'a FactoryStatus)>,
}

/// Each project of `settings` whose factory is on.
fn factory_on(settings: &SharedSettings) -> impl Iterator<Item = &Project> {
    settings.projects.iter().filter(|project| {
        settings
            .project_factory
            .get(&project.id)
            .is_some_and(|f| f.on)
    })
}

/// Each project whose factory is on, with its believed status.
fn on_projects<'a>(
    fleet: &'a Fleet,
    factory: &'a FactoryFleet<'_>,
    acting: MachineId,
) -> Vec<OnProject<'a>> {
    factory_on(&fleet.settings)
        .map(|project| {
            let statuses = factory
                .statuses
                .get(&project.id)
                .map_or(&[][..], Vec::as_slice);
            OnProject {
                project,
                believed: believed(statuses, &fleet.online, acting),
            }
        })
        .collect()
}

/// The sessions and workspaces whose P1 items are left out because they are factory work (G13).
#[derive(Default)]
struct FactoryWork {
    sessions: BTreeSet<SessionId>,
    workspaces: BTreeSet<WorkspaceId>,
}

impl FactoryWork {
    /// For each project whose factory is on: what a status of it at the highest lease
    /// generation any of its statuses gives names in a live run, so a holder that went offline
    /// names its runs' work until a newer holder takes over and a superseded writer names
    /// nothing; and what this machine marks of its own records as the project's. Then each
    /// workspace that holds such a session.
    fn of(fleet: &Fleet, factory: &FactoryFleet<'_>, on: &[OnProject<'_>]) -> Self {
        let mut work = Self::default();
        let on_ids: BTreeSet<ProjectId> = on.iter().map(|p| p.project.id).collect();
        for project in &on_ids {
            let statuses = factory.statuses.get(project).map_or(&[][..], Vec::as_slice);
            // A status that does not hold the lease carries the generation the lease ref names
            // when another machine holds it, so a newer holder seen but not heard from counts.
            let top = statuses.iter().map(|(_, status)| status.generation).max();
            let naming = statuses
                .iter()
                .filter(|(_, status)| Some(status.generation) == top)
                .map(|(_, status)| status);
            for run in naming.flat_map(|status| &status.runs) {
                work.sessions.extend(&run.sessions);
                work.workspaces.extend(&run.workspaces);
            }
        }
        if let Some(marks) = factory.marks {
            let ours = |project: Option<ProjectId>| project.is_some_and(|p| on_ids.contains(&p));
            for record in &fleet.records {
                match record {
                    AnyRecord::Session(r)
                        if r.owner == fleet.me && ours(marks.factory_session(r.body.id)) =>
                    {
                        work.sessions.insert(r.body.id);
                    }
                    AnyRecord::Workspace(r)
                        if r.owner == fleet.me && ours(marks.factory_workspace(r.body.id)) =>
                    {
                        work.workspaces.insert(r.body.id);
                    }
                    _ => {}
                }
            }
        }
        for record in &fleet.records {
            if let AnyRecord::Session(r) = record
                && work.sessions.contains(&r.body.id)
            {
                work.workspaces.insert(r.body.workspace);
            }
        }
        work
    }

    fn session(&self, session: &Session) -> bool {
        self.sessions.contains(&session.id)
    }

    fn workspace(&self, workspace: WorkspaceId) -> bool {
        self.workspaces.contains(&workspace)
    }
}

/// The items the synced records, the shared settings and the factory statuses raise: every
/// row but the gateway's and placement's. A session or workspace that is factory work raises
/// none of P1's session or setup items.
pub fn fleet_items(fleet: &Fleet, factory: &FactoryFleet<'_>) -> Vec<Item> {
    let acting = coordinator::acting(&fleet.settings.priority, &fleet.online);
    let on = on_projects(fleet, factory, acting);
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

    let work = FactoryWork::of(fleet, factory, &on);
    for record in &fleet.records {
        match record {
            AnyRecord::Session(r) if !r.deleted && !work.session(&r.body) => {
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
            AnyRecord::Workspace(r) if !r.deleted && !work.workspace(r.body.id) => {
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
            SettingKey::Factory => "the factory settings".to_owned(),
            SettingKey::ProjectFactory(id) => {
                match fleet.settings.projects.iter().find(|p| p.id == *id) {
                    Some(project) => format!("the factory of the project {}", project.name),
                    None => "the factory of a project".to_owned(),
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

    items.extend(factory_items(fleet, factory, &on, acting, &closed));
    items
}

/// The factory's rows, for each project whose factory is on.
fn factory_items(
    fleet: &Fleet,
    factory: &FactoryFleet<'_>,
    on: &[OnProject<'_>],
    acting: MachineId,
    closed: &dyn Fn(TerminalId) -> bool,
) -> Vec<Item> {
    let quiet = factory
        .quiet
        .map(|quiet| quiet.of(&fleet.settings, &on.iter().map(|p| p.project.id).collect()))
        .unwrap_or_default();
    // The sessions a held-up run names, found only when some status holds a run up: a session's
    // terminal is offered when its record is held, not deleted, and its terminal is not closed
    // (`closed`, the one the "session failed" row uses).
    let sessions = std::cell::LazyCell::new(|| {
        fleet
            .records
            .iter()
            .filter_map(|record| match record {
                AnyRecord::Session(r) if !r.deleted => Some((r.body.id, r.body.terminal)),
                _ => None,
            })
            .collect::<BTreeMap<SessionId, TerminalId>>()
    });
    let mut items = Vec::new();
    for &OnProject { project, believed } in on {
        let quiet = quiet
            .get(&project.id)
            .is_some_and(|quiet| *quiet >= IDLE_AFTER);
        if let Some(item) = idle(fleet, project.id, &project.name, believed, quiet, acting) {
            items.push(item);
        }
        let Some((writer, status)) = believed else {
            continue;
        };
        for held in &status.held_up {
            let (kind, what, todo) = match held.kind {
                HeldUpKind::Stuck => (
                    Kind::RunStuck,
                    format!("The factory's run of {} is stuck", issue(&held.run)),
                    "Read the factory's last comment on the issue, fix the cause, then move the \
                     issue out of factory:stuck",
                ),
                HeldUpKind::NeedsInfo => (
                    Kind::RunNeedsInfo,
                    format!("The factory's run of {} asked questions", issue(&held.run)),
                    "Answer them in a comment on the issue",
                ),
            };
            let mut item = fleet.item(
                kind,
                &held.run,
                writer,
                format!("{what}: {}", redact(&held.reason)),
                todo.to_owned(),
            );
            item.issue_url = Some(issue_url(&held.run));
            item.pr_url = held
                .pr_url
                .clone()
                .filter(|url| url.starts_with("https://github.com/"));
            item.terminal = held
                .sessions
                .iter()
                .filter_map(|id| sessions.get(id).copied())
                .find(|&terminal| !closed(terminal));
            items.push(item);
        }
        for run in &status.gate_infra {
            let mut item = fleet.item(
                Kind::GateInfra,
                run,
                writer,
                format!(
                    "A gate of the factory's run of {} could not run at all",
                    issue(run)
                ),
                "Check that a machine that holds the project's GitHub login is online with free \
                 disk and can fetch the repo; the factory tries the gate again after 1, 2, 4 and \
                 8 passes"
                    .to_owned(),
            );
            item.issue_url = Some(issue_url(run));
            items.push(item);
        }
        for claim in &status.foreign {
            items.push(fleet.item(
                Kind::ForeignClaim,
                format!("{}__{}", project.id, claim.issue),
                writer,
                format!(
                    "Issue #{} of {}, {}, is in factory:{} under a claim that is not Foundry's",
                    claim.issue,
                    project.name,
                    redact(&claim.title),
                    claim.state
                ),
                format!(
                    "Foundry leaves it alone: let whatever claimed it finish, or move the issue \
                     out of factory:{}",
                    claim.state
                ),
            ));
        }
    }
    items
}

/// The GitHub page of `run`'s issue.
fn issue_url(run: &RunId) -> String {
    format!("https://github.com/{}/issues/{}", run.repo(), run.issue())
}

/// `owner/name#N` of `run`'s issue.
fn issue(run: &RunId) -> String {
    format!("{}#{}", run.repo(), run.issue())
}

/// The "factory idle" item of `project`, when it stands: for a reason the believed status
/// gives, else when this machine has gone [`IDLE_AFTER`] with neither the lease nor word from
/// its holder (`quiet`).
fn idle(
    fleet: &Fleet,
    project: ProjectId,
    name: &str,
    believed: Option<(MachineId, &FactoryStatus)>,
    quiet: bool,
    acting: MachineId,
) -> Option<Item> {
    let item = |machine, what: String, todo: String| {
        Some(fleet.item(Kind::FactoryIdle, project, machine, what, todo))
    };
    match believed {
        Some((writer, status)) if status.holds => {
            if let Some(reason) = &status.idle {
                return item(
                    writer,
                    format!("The factory of {name} is idle: {}", redact(reason)),
                    "Fix what it names; the factory starts again on its next pass".to_owned(),
                );
            }
        }
        Some((writer, status)) => {
            if let Some(NotHolding::NoToken { login }) = &status.not_holding {
                let on = fleet.name(writer);
                return item(
                    writer,
                    format!(
                        "The factory of {name} is idle: {on}, the acting coordinator, holds no \
                         token for the GitHub login {login}"
                    ),
                    format!("Run `foundryd secret set github:{login}` on {on}"),
                );
            }
        }
        None => {}
    }
    if !quiet {
        return None;
    }
    let minutes = IDLE_AFTER.as_secs() / 60;
    let todo = "Check that the acting coordinator is on, speaks protocol 5 and can reach \
                GitHub"
        .to_owned();
    match believed.and_then(|(_, status)| status.not_holding.as_ref()) {
        Some(NotHolding::HeldBy {
            machine,
            generation,
        }) => item(
            *machine,
            format!(
                "The factory of {name} has had no word for {minutes} minutes from {}, which \
                 holds its lease at generation {generation}",
                fleet.name(*machine)
            ),
            todo,
        ),
        _ => item(
            acting,
            format!(
                "The factory of {name} has had no word from a holder of its lease for \
                 {minutes} minutes"
            ),
            todo,
        ),
    }
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
        Some(SettingValue::Factory(factory)) => format!(
            "{}{} runs, {} gates, a pass every {} s, feedback every {} s, {} at {} effort",
            if factory.paused {
                "claims paused, "
            } else {
                ""
            },
            factory.max_runs,
            factory.max_gates,
            factory.poll_seconds,
            factory.feedback_seconds,
            factory.model,
            factory.effort
        ),
        Some(SettingValue::ProjectFactory(factory)) => {
            let mut text = if factory.on { "on" } else { "off" }.to_owned();
            if !factory.login.is_empty() {
                text.push_str(&format!(" as {}", factory.login));
            }
            if factory.paused {
                text.push_str(", claims paused");
            }
            text
        }
    }
}

/// What names a record among its owner's.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Workspace(WorkspaceId),
    Terminal(TerminalId),
    Session(SessionId),
    /// An owner has one sample.
    Sample,
}

impl Key {
    fn of(record: &AnyRecord) -> Self {
        match record {
            AnyRecord::Workspace(r) => Self::Workspace(r.body.id),
            AnyRecord::Terminal(r) => Self::Terminal(r.body.id),
            AnyRecord::Session(r) => Self::Session(r.body.id),
            AnyRecord::MachineSample(_) => Self::Sample,
        }
    }
}

/// The records read so far, kept from one read of the list to the next, so that a read loads
/// only what was written since the last one.
#[derive(Default)]
struct Held {
    /// The highest version read from each owner.
    seen: BTreeMap<MachineId, u64>,
    /// The newest copy of each owner's records, deleted ones too: a closed terminal is what
    /// clears a "session failed" item.
    records: BTreeMap<MachineId, BTreeMap<Key, AnyRecord>>,
}

impl Held {
    /// Takes in what `store` holds above the versions already read, and says how many records
    /// that was. An owner with a record this build cannot read, one a newer build wrote before
    /// a rollback, is left out whole with a warning, as [`Peers::state`] leaves it out, and is
    /// read from its first record again on the next read.
    fn load(&mut self, store: &Store) -> Result<usize, StoreError> {
        let mut read = 0;
        for (owner, newest) in store.versions()? {
            let seen = self.seen.get(&owner).copied().unwrap_or(0);
            if newest <= seen {
                continue;
            }
            match store.since(owner, seen) {
                Ok(records) => {
                    read += records.len();
                    let held = self.records.entry(owner).or_default();
                    for record in records {
                        held.insert(Key::of(&record), record);
                    }
                    self.seen.insert(owner, newest);
                }
                Err(error) => {
                    tracing::warn!("attention: records of {owner} not shown: {error}");
                    self.seen.remove(&owner);
                    self.records.remove(&owner);
                }
            }
        }
        Ok(read)
    }

    /// Every record held, with the sessions and terminals of an owner not in `online` shown
    /// as unreachable.
    fn as_seen(&self, online: &BTreeSet<MachineId>) -> Vec<AnyRecord> {
        self.records
            .values()
            .flat_map(BTreeMap::values)
            .map(|record| as_seen(record.clone(), online))
            .collect()
    }
}

/// The attention list of one machine: the fleet's items and those of each [`Source`].
pub struct Attention {
    me: MachineId,
    store: Arc<Mutex<Store>>,
    peers: Peers,
    coordinator: Arc<Coordinator>,
    sources: Vec<Arc<dyn Source>>,
    held: Arc<Mutex<Held>>,
    quiet: Option<Arc<Quiet>>,
    marks: Option<Arc<dyn Marks>>,
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
            held: Arc::default(),
            quiet: None,
            marks: None,
        }
    }

    /// Adds `source`'s items to the list.
    pub fn with_source(mut self, source: Arc<dyn Source>) -> Self {
        self.sources.push(source);
        self
    }

    /// Reads this machine's `freshness`, which counts on `clock`, for "factory idle", and its
    /// `marks` for the factory work whose P1 items are left out. Without them the factory's
    /// items are still read from the statuses.
    pub fn with_factory(
        mut self,
        clock: Arc<dyn Clock>,
        freshness: Arc<Freshness>,
        marks: Arc<dyn Marks>,
    ) -> Self {
        self.quiet = Some(Arc::new(Quiet::new(clock, freshness)));
        self.marks = Some(marks);
        self
    }

    /// Every item that stands now, ordered by kind and then id. Only the records written since
    /// the last read are loaded, but the factory statuses of each project whose factory is on
    /// are read in full on every call. Records are shown as [`Peers::state`] shows them, so an offline
    /// machine's sessions and terminals are unreachable and raise nothing but its "machine
    /// offline" item.
    pub async fn items(&self) -> Result<Vec<Item>, StoreError> {
        let online = self.peers.online();
        let (records, settings, statuses) = on_store(&self.store, {
            let (held, online) = (Arc::clone(&self.held), online.clone());
            move |store| {
                let mut held = lock(&held);
                held.load(store)?;
                let settings = store.settings()?;
                let mut statuses = BTreeMap::new();
                for project in factory_on(&settings) {
                    statuses.insert(project.id, store.statuses(project.id)?);
                }
                Ok::<_, StoreError>((held.as_seen(&online), settings, statuses))
            }
        })
        .await?;
        let fleet = Fleet {
            me: self.me,
            records,
            settings,
            online,
        };
        let factory = FactoryFleet {
            statuses,
            quiet: self.quiet.as_deref(),
            marks: self.marks.as_deref(),
        };
        let mut items = fleet_items(&fleet, &factory);
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
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => error(edit_status(&e), e.to_string()),
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
    use foundry_proto::settings::{
        Limits, Machine, Outcome, Overwritten, Project, Stamp, StampKey,
    };
    use futures::future::BoxFuture;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use foundry_proto::factory::{
        FactoryBody, FactoryRecord, FactoryStatus, ForeignClaim, HeldUp, HeldUpKind, LiveRun,
        NotHolding, ProjectFactory, RunId,
    };

    use super::*;
    use crate::clock::{Clock, FakeClock};
    use crate::coordinator::CoordinatorError;
    use crate::factory::status::{Freshness, PassResults, build, not_holding};
    use crate::sync::{Conn, Dialer};

    const GB: u64 = 1_000_000_000;

    /// What this machine marks as factory work, in a test.
    #[derive(Default)]
    struct Marked {
        sessions: BTreeMap<SessionId, ProjectId>,
        workspaces: BTreeMap<WorkspaceId, ProjectId>,
    }

    impl Marks for Marked {
        fn factory_session(&self, session: SessionId) -> Option<ProjectId> {
            self.sessions.get(&session).copied()
        }

        fn factory_workspace(&self, workspace: WorkspaceId) -> Option<ProjectId> {
            self.workspaces.get(&workspace).copied()
        }
    }

    /// This machine and one peer, both named and both heard from, holding nothing yet.
    struct World {
        me: MachineId,
        peer: MachineId,
        records: Vec<AnyRecord>,
        settings: SharedSettings,
        online: BTreeSet<MachineId>,
        version: u64,
        /// Every writer's status of each project.
        statuses: BTreeMap<ProjectId, Vec<(MachineId, FactoryStatus)>>,
        /// This machine's freshness and quiet on a clock the test moves, when it keeps them.
        quiet: Option<(Arc<Freshness>, Quiet)>,
        marked: Marked,
    }

    impl World {
        fn new() -> Self {
            let (me, peer) = (MachineId::random(), MachineId::random());
            let machine = |id, name: &str| Machine {
                id,
                name: name.into(),
                address: "100.64.0.1".into(),
                key: String::new(),
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
                statuses: BTreeMap::new(),
                quiet: None,
                marked: Marked::default(),
            }
        }

        /// A project named `foundry` whose factory is `on`, with this machine first in the
        /// coordinator order, so it acts.
        fn factory_project(&mut self, on: bool) -> ProjectId {
            let id = with_factory_on(self.me, &mut self.settings);
            self.turn(id, on);
            self.settings.priority = vec![self.me, self.peer];
            id
        }

        /// Turns `project`'s factory `on` or off, with a new stamp, as an edit does.
        fn turn(&mut self, project: ProjectId, on: bool) {
            self.settings
                .project_factory
                .get_mut(&project)
                .expect("a factory project")
                .on = on;
            self.settings.seq += 1;
            self.settings.stamps.insert(
                StampKey::Setting(SettingKey::ProjectFactory(project)),
                Stamp {
                    seq: self.settings.seq,
                    by: self.me,
                },
            );
        }

        /// Puts `status` in place of any status `writer` wrote of its project.
        fn status(&mut self, writer: MachineId, status: FactoryStatus) {
            let held = self.statuses.entry(status.project).or_default();
            held.retain(|(w, _)| *w != writer);
            held.push((writer, status));
        }

        /// Gives this machine a freshness and a quiet on a fake clock, and returns the clock.
        fn keep_freshness(&mut self) -> Arc<FakeClock> {
            let (clock, freshness, quiet) = fake_quiet();
            self.quiet = Some((freshness, quiet));
            clock
        }

        fn freshness(&self) -> &Freshness {
            &self.quiet.as_ref().expect("keep_freshness was called").0
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
                name: None,
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
                version: None,
                update_ready: None,
            };
            let record = self.record(owner, false, body);
            self.put(AnyRecord::MachineSample(record));
        }

        fn items(&self) -> Vec<Item> {
            let fleet = Fleet {
                me: self.me,
                records: self.records.clone(),
                settings: self.settings.clone(),
                online: self.online.clone(),
            };
            let factory = FactoryFleet {
                statuses: self.statuses.clone(),
                quiet: self.quiet.as_ref().map(|(_, q)| q),
                marks: Some(&self.marked),
            };
            fleet_items(&fleet, &factory)
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
            version: None,
            update_ready: None,
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
    fn an_overwritten_factory_setting_reads_plainly() {
        use foundry_proto::factory::{FactorySettings, ProjectFactory};

        let mut world = World::new();
        let (me, peer) = (world.me, world.peer);
        let project = Project {
            id: ProjectId::random(),
            name: "foundry".into(),
            repo: "https://github.com/o/foundry.git".into(),
        };
        world.settings.projects.push(project.clone());
        world.settings.overwritten.insert(
            SettingKey::Factory,
            Overwritten {
                kept: Some(SettingValue::Factory(FactorySettings {
                    paused: true,
                    ..FactorySettings::default()
                })),
                lost: Some(SettingValue::Factory(FactorySettings::default())),
                lost_stamp: Stamp { seq: 2, by: peer },
            },
        );
        world.settings.overwritten.insert(
            SettingKey::ProjectFactory(project.id),
            Overwritten {
                kept: Some(SettingValue::ProjectFactory(ProjectFactory {
                    on: true,
                    login: "rawphp".into(),
                    paused: false,
                })),
                lost: Some(SettingValue::ProjectFactory(ProjectFactory {
                    on: false,
                    login: String::new(),
                    paused: true,
                })),
                lost_stamp: Stamp { seq: 2, by: me },
            },
        );
        let gone = ProjectId::random();
        world.settings.overwritten.insert(
            SettingKey::ProjectFactory(gone),
            Overwritten {
                kept: None,
                lost: Some(SettingValue::ProjectFactory(ProjectFactory::default())),
                lost_stamp: Stamp { seq: 2, by: me },
            },
        );
        let items = world.of(Kind::SettingOverwritten);
        assert_eq!(items.len(), 3, "{items:?}");
        let fleet = items.iter().find(|i| i.id.ends_with(":factory")).unwrap();
        assert!(
            fleet.what.contains(
                "edited the factory settings: kept claims paused, 3 runs, 2 gates, a pass \
                 every 30 s, feedback every 600 s, opus at high effort; replaced 3 runs, 2 \
                 gates, a pass every 30 s, feedback every 600 s, opus at high effort"
            ),
            "{}",
            fleet.what
        );
        let one = items
            .iter()
            .find(|i| i.id.ends_with(&format!("factory:{}", project.id)))
            .unwrap();
        assert!(
            one.what.contains(
                "edited the factory of the project foundry: kept on as rawphp; replaced off, \
                 claims paused"
            ),
            "{}",
            one.what
        );
        let unknown = items
            .iter()
            .find(|i| i.id.ends_with(&format!("factory:{gone}")))
            .unwrap();
        assert!(
            unknown
                .what
                .contains("edited the factory of a project: kept removed; replaced off"),
            "{}",
            unknown.what
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

    // --- factory items ---

    const MINUTE: Duration = Duration::from_secs(60);

    /// A fake clock, a freshness on it and a quiet that reads both.
    fn fake_quiet() -> (Arc<FakeClock>, Arc<Freshness>, Quiet) {
        let clock = Arc::new(FakeClock::new());
        let dyn_clock: Arc<dyn Clock> = clock.clone();
        let freshness = Arc::new(Freshness::new(dyn_clock.clone()));
        let quiet = Quiet::new(dyn_clock, freshness.clone());
        (clock, freshness, quiet)
    }

    fn run(issue: u64) -> RunId {
        RunId::new("o/foundry", issue, 1).unwrap()
    }

    /// A status that holds `project`'s lease at `generation`, after pass `pass`.
    fn holding(project: ProjectId, generation: u64, pass: u64) -> FactoryStatus {
        build(PassResults {
            generation,
            pass,
            ..PassResults::new(project)
        })
    }

    fn held_up(issue: u64, kind: HeldUpKind, reason: &str) -> HeldUp {
        HeldUp {
            run: run(issue),
            kind,
            reason: reason.into(),
            workspaces: vec![],
            pr_url: None,
            sessions: vec![],
        }
    }

    // run stuck

    #[test]
    fn a_stuck_run_in_the_believed_status_raises_run_stuck() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let mut status = holding(project, 3, 7);
        status
            .held_up
            .push(held_up(12, HeldUpKind::Stuck, "the gate failed 3 times"));
        world.status(world.peer, status);
        let items = world.of(Kind::RunStuck);
        assert_eq!(items.len(), 1, "{items:?}");
        let item = &items[0];
        assert_eq!(item.id, format!("run_stuck:{}", run(12)));
        assert_eq!(item.machine, world.peer);
        assert_eq!(item.machine_name, "trilogy-mac");
        assert!(item.what.contains("o/foundry#12"), "{}", item.what);
        assert!(
            item.what.contains("the gate failed 3 times"),
            "{}",
            item.what
        );
        assert!(!item.todo.is_empty());
        assert!(world.of(Kind::RunNeedsInfo).is_empty());
    }

    #[test]
    fn an_issue_that_leaves_stuck_clears_run_stuck() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let mut status = holding(project, 3, 7);
        status.held_up.push(held_up(12, HeldUpKind::Stuck, "no"));
        world.status(world.peer, status);
        assert_eq!(world.of(Kind::RunStuck).len(), 1);
        world.status(world.peer, holding(project, 3, 8));
        assert!(world.of(Kind::RunStuck).is_empty());
    }

    // run needs info

    #[test]
    fn a_run_that_posts_questions_raises_run_needs_info() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let mut status = holding(project, 3, 7);
        status
            .held_up
            .push(held_up(14, HeldUpKind::NeedsInfo, "which login"));
        world.status(world.peer, status);
        let items = world.of(Kind::RunNeedsInfo);
        assert_eq!(items.len(), 1, "{items:?}");
        let item = &items[0];
        assert_eq!(item.id, format!("run_needs_info:{}", run(14)));
        assert_eq!(item.machine, world.peer);
        assert!(item.what.contains("o/foundry#14"), "{}", item.what);
        assert!(item.what.contains("which login"), "{}", item.what);
        assert!(world.of(Kind::RunStuck).is_empty());
    }

    #[test]
    fn an_issue_that_leaves_needs_info_clears_run_needs_info() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let mut status = holding(project, 3, 7);
        status
            .held_up
            .push(held_up(14, HeldUpKind::NeedsInfo, "which login"));
        world.status(world.peer, status);
        assert_eq!(world.of(Kind::RunNeedsInfo).len(), 1);
        world.status(world.peer, holding(project, 3, 8));
        assert!(world.of(Kind::RunNeedsInfo).is_empty());
    }

    /// A status with one stuck run of issue 12 that reports `pr_url` and ran in `sessions`.
    fn stuck_with(world: &mut World, pr_url: Option<&str>, sessions: Vec<SessionId>) {
        let project = world.factory_project(true);
        let mut status = holding(project, 3, 7);
        let mut held = held_up(12, HeldUpKind::Stuck, "no");
        held.pr_url = pr_url.map(str::to_owned);
        held.sessions = sessions;
        status.held_up.push(held);
        world.status(world.peer, status);
    }

    #[test]
    fn a_run_stuck_or_needing_info_links_its_issue() {
        let mut world = World::new();
        stuck_with(&mut world, None, vec![]);
        let items = world.of(Kind::RunStuck);
        assert_eq!(
            items[0].issue_url.as_deref(),
            Some("https://github.com/o/foundry/issues/12")
        );
        assert_eq!(items[0].pr_url, None);
        let mut world = World::new();
        let project = world.factory_project(true);
        let mut status = holding(project, 3, 7);
        status
            .held_up
            .push(held_up(14, HeldUpKind::NeedsInfo, "which login"));
        world.status(world.peer, status);
        assert_eq!(
            world.of(Kind::RunNeedsInfo)[0].issue_url.as_deref(),
            Some("https://github.com/o/foundry/issues/14")
        );
    }

    #[test]
    fn a_run_stuck_links_the_pull_request_it_reported() {
        let mut world = World::new();
        let url = "https://github.com/o/foundry/pull/40";
        stuck_with(&mut world, Some(url), vec![]);
        assert_eq!(world.of(Kind::RunStuck)[0].pr_url.as_deref(), Some(url));
    }

    #[test]
    fn a_run_stuck_with_no_pull_request_links_none() {
        let mut world = World::new();
        stuck_with(&mut world, None, vec![]);
        assert_eq!(world.of(Kind::RunStuck)[0].pr_url, None);
    }

    #[test]
    fn a_pull_request_url_not_on_github_is_not_linked() {
        for url in [
            "https://evil.example/pull/1",
            "javascript:alert(1)",
            "http://github.com/o/r/pull/1",
        ] {
            let mut world = World::new();
            stuck_with(&mut world, Some(url), vec![]);
            assert_eq!(world.of(Kind::RunStuck)[0].pr_url, None, "{url}");
        }
    }

    #[test]
    fn a_run_stuck_opens_the_terminal_of_its_session_while_it_is_open() {
        let mut world = World::new();
        let peer = world.peer;
        let session = world.session(peer, SessionState::Working);
        stuck_with(&mut world, None, vec![session.id]);
        assert_eq!(world.of(Kind::RunStuck)[0].terminal, Some(session.terminal));
    }

    #[test]
    fn a_run_stuck_takes_the_first_session_that_has_an_open_terminal() {
        let mut world = World::new();
        let peer = world.peer;
        let gone = SessionId::random();
        let closed = world.session(peer, SessionState::Working);
        world.terminal(peer, closed.terminal, closed.workspace, None, true);
        let open = world.session(peer, SessionState::Working);
        let later = world.session(peer, SessionState::Working);
        stuck_with(&mut world, None, vec![gone, closed.id, open.id, later.id]);
        assert_eq!(world.of(Kind::RunStuck)[0].terminal, Some(open.terminal));
    }

    #[test]
    fn a_run_stuck_has_no_terminal_when_its_session_record_is_missing_or_deleted_or_closed() {
        let mut world = World::new();
        stuck_with(&mut world, None, vec![SessionId::random()]);
        assert_eq!(world.of(Kind::RunStuck)[0].terminal, None);

        let mut world = World::new();
        let peer = world.peer;
        let session = world.session(peer, SessionState::Working);
        world.terminal(peer, session.terminal, session.workspace, None, true);
        stuck_with(&mut world, None, vec![session.id]);
        assert_eq!(world.of(Kind::RunStuck)[0].terminal, None);

        let mut world = World::new();
        let peer = world.peer;
        let session = world.session(peer, SessionState::Working);
        world.put_session(peer, session.clone(), true);
        stuck_with(&mut world, None, vec![session.id]);
        assert_eq!(world.of(Kind::RunStuck)[0].terminal, None);
    }

    #[test]
    fn a_run_needing_info_opens_its_terminal_and_links_its_pull_request() {
        let mut world = World::new();
        let peer = world.peer;
        let session = world.session(peer, SessionState::Working);
        let project = world.factory_project(true);
        let mut status = holding(project, 3, 7);
        let mut held = held_up(14, HeldUpKind::NeedsInfo, "which login");
        held.pr_url = Some("https://github.com/o/foundry/pull/9".into());
        held.sessions = vec![session.id];
        status.held_up.push(held);
        world.status(peer, status);
        let item = &world.of(Kind::RunNeedsInfo)[0];
        assert_eq!(item.terminal, Some(session.terminal));
        assert_eq!(
            item.pr_url.as_deref(),
            Some("https://github.com/o/foundry/pull/9")
        );
    }

    #[test]
    fn a_gate_infra_item_links_its_issue_and_other_kinds_link_nothing() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let mut status = holding(project, 3, 7);
        status.gate_infra.push(run(30));
        world.status(world.peer, status);
        let item = &world.of(Kind::GateInfra)[0];
        assert_eq!(
            item.issue_url.as_deref(),
            Some("https://github.com/o/foundry/issues/30")
        );
        assert_eq!(item.pr_url, None);
        let mut world = World::new();
        world.online.remove(&world.peer);
        let item = &world.of(Kind::MachineOffline)[0];
        assert_eq!((&item.issue_url, &item.pr_url), (&None, &None));
        let json = serde_json::to_value(item).unwrap();
        assert!(
            json["issue_url"].is_null() && json["pr_url"].is_null(),
            "{json}"
        );
    }

    // factory idle

    #[test]
    fn a_reason_in_the_holders_status_raises_factory_idle() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let mut status = holding(project, 3, 7);
        status.idle = Some("gh failed three passes running".into());
        world.status(world.peer, status);
        let items = world.of(Kind::FactoryIdle);
        assert_eq!(items.len(), 1, "{items:?}");
        let item = &items[0];
        assert_eq!(item.id, format!("factory_idle:{project}"));
        assert_eq!(item.machine, world.peer);
        assert!(item.what.contains("foundry"), "{}", item.what);
        assert!(
            item.what.contains("gh failed three passes running"),
            "{}",
            item.what
        );
    }

    #[test]
    fn the_reason_going_clears_factory_idle() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let mut status = holding(project, 3, 7);
        status.idle = Some(".dw/config.yml does not load".into());
        world.status(world.peer, status);
        assert_eq!(world.of(Kind::FactoryIdle).len(), 1);
        world.status(world.peer, holding(project, 3, 8));
        assert!(world.of(Kind::FactoryIdle).is_empty());
    }

    #[test]
    fn a_coordinator_with_no_token_raises_factory_idle_naming_the_login() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let status = not_holding(
            project,
            NotHolding::NoToken {
                login: "octocat".into(),
            },
        );
        world.status(world.me, status);
        let items = world.of(Kind::FactoryIdle);
        assert_eq!(items.len(), 1, "{items:?}");
        let item = &items[0];
        assert_eq!(item.machine, world.me);
        assert!(item.what.contains("octocat"), "{}", item.what);
        assert!(item.todo.contains("github:octocat"), "{}", item.todo);

        // A holder heard from again clears it.
        world.status(world.peer, holding(project, 1, 1));
        assert!(world.of(Kind::FactoryIdle).is_empty());
    }

    #[test]
    fn ten_minutes_with_no_holding_status_raises_factory_idle_for_a_project_whose_factory_is_on() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let clock = world.keep_freshness();
        clock.advance(10 * MINUTE - Duration::from_secs(1));
        assert!(world.of(Kind::FactoryIdle).is_empty());
        clock.advance(Duration::from_secs(1));
        let items = world.of(Kind::FactoryIdle);
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].id, format!("factory_idle:{project}"));
        assert!(items[0].what.contains("10 minutes"), "{}", items[0].what);
    }

    #[test]
    fn a_status_that_does_not_hold_the_lease_for_ten_minutes_raises_factory_idle() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let clock = world.keep_freshness();
        let peer = world.peer;
        let status = not_holding(
            project,
            NotHolding::HeldBy {
                machine: peer,
                generation: 4,
            },
        );
        world.status(world.me, status);
        world.freshness().received(&world.statuses[&project][0].1);
        clock.advance(9 * MINUTE);
        assert!(world.of(Kind::FactoryIdle).is_empty());
        clock.advance(MINUTE);
        let items = world.of(Kind::FactoryIdle);
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].machine, world.peer);
        assert!(items[0].what.contains("trilogy-mac"), "{}", items[0].what);
    }

    #[test]
    fn a_project_whose_factory_is_off_raises_no_factory_idle_after_ten_minutes() {
        let mut world = World::new();
        world.factory_project(false);
        let clock = world.keep_freshness();
        clock.advance(60 * MINUTE);
        assert!(
            world.of(Kind::FactoryIdle).is_empty(),
            "{:?}",
            world.items()
        );
    }

    #[test]
    fn holding_the_lease_or_hearing_from_its_holder_clears_factory_idle() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let clock = world.keep_freshness();
        clock.advance(10 * MINUTE);
        assert_eq!(world.of(Kind::FactoryIdle).len(), 1);
        world.freshness().held(project);
        assert!(world.of(Kind::FactoryIdle).is_empty());

        clock.advance(10 * MINUTE);
        assert_eq!(world.of(Kind::FactoryIdle).len(), 1);
        let status = holding(project, 2, 9);
        world.freshness().received(&status);
        world.status(world.peer, status);
        assert!(world.of(Kind::FactoryIdle).is_empty());
    }

    #[test]
    fn a_first_read_long_after_the_start_raises_factory_idle_at_once() {
        let mut world = World::new();
        world.factory_project(true);
        let clock = world.keep_freshness();
        clock.advance(30 * MINUTE);
        assert_eq!(world.of(Kind::FactoryIdle).len(), 1, "{:?}", world.items());
    }

    #[test]
    fn a_factory_turned_off_and_on_between_two_reads_counts_from_the_second() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let clock = world.keep_freshness();
        assert!(world.of(Kind::FactoryIdle).is_empty());
        clock.advance(60 * MINUTE);
        world.turn(project, false);
        world.turn(project, true);
        assert!(
            world.of(Kind::FactoryIdle).is_empty(),
            "{:?}",
            world.items()
        );
        clock.advance(10 * MINUTE);
        assert_eq!(world.of(Kind::FactoryIdle).len(), 1);
    }

    #[test]
    fn a_factory_setting_first_seen_after_a_read_counts_from_that_read() {
        let mut world = World::new();
        let first = world.factory_project(true);
        let clock = world.keep_freshness();
        let idle = |world: &World| {
            let mut ids: Vec<String> = world
                .of(Kind::FactoryIdle)
                .into_iter()
                .map(|i| i.id)
                .collect();
            ids.sort();
            ids
        };
        assert!(idle(&world).is_empty());
        clock.advance(30 * MINUTE);
        // A second project's factory setting arrives after that read, already on. The first
        // project, there at the first read, still counts from the start.
        let second = world.factory_project(true);
        assert_eq!(idle(&world), [format!("factory_idle:{first}")]);
        clock.advance(9 * MINUTE);
        assert_eq!(idle(&world), [format!("factory_idle:{first}")]);
        clock.advance(MINUTE);
        let mut both = vec![
            format!("factory_idle:{first}"),
            format!("factory_idle:{second}"),
        ];
        both.sort();
        assert_eq!(idle(&world), both);
    }

    #[test]
    fn a_holder_that_is_believed_but_sends_nothing_new_for_ten_minutes_raises_factory_idle() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let clock = world.keep_freshness();
        let status = holding(project, 2, 9);
        world.freshness().received(&status);
        world.status(world.peer, status);
        clock.advance(10 * MINUTE);
        let items = world.of(Kind::FactoryIdle);
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].machine, world.me);
        assert!(items[0].what.contains("10 minutes"), "{}", items[0].what);

        let status = holding(project, 2, 10);
        world.freshness().received(&status);
        world.status(world.peer, status);
        assert!(world.of(Kind::FactoryIdle).is_empty());
    }

    #[test]
    fn the_ten_minutes_count_from_when_the_factory_is_seen_on() {
        let mut world = World::new();
        let project = world.factory_project(false);
        let clock = world.keep_freshness();
        clock.advance(60 * MINUTE);
        assert!(world.of(Kind::FactoryIdle).is_empty());
        world.turn(project, true);
        assert!(
            world.of(Kind::FactoryIdle).is_empty(),
            "{:?}",
            world.items()
        );
        clock.advance(9 * MINUTE);
        assert!(
            world.of(Kind::FactoryIdle).is_empty(),
            "{:?}",
            world.items()
        );
        clock.advance(MINUTE);
        assert_eq!(world.of(Kind::FactoryIdle).len(), 1);

        // Off and on again: the count starts again.
        world.turn(project, false);
        assert!(world.of(Kind::FactoryIdle).is_empty());
        world.turn(project, true);
        assert!(
            world.of(Kind::FactoryIdle).is_empty(),
            "{:?}",
            world.items()
        );
    }

    #[test]
    fn no_token_names_the_acting_coordinator_when_another_writer_says_the_same() {
        let mut world = World::new();
        let project = world.factory_project(true);
        // The peer was acting before this machine and wrote the same status.
        for writer in [world.peer, world.me] {
            let status = not_holding(
                project,
                NotHolding::NoToken {
                    login: "octocat".into(),
                },
            );
            world.status(writer, status);
        }
        world.settings.priority = vec![world.me, world.peer];
        let items = world.of(Kind::FactoryIdle);
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].machine, world.me);
        assert!(items[0].todo.contains("this-mac"), "{}", items[0].todo);

        world.settings.priority = vec![world.peer, world.me];
        let items = world.of(Kind::FactoryIdle);
        assert_eq!(items[0].machine, world.peer);
        assert!(items[0].todo.contains("trilogy-mac"), "{}", items[0].todo);
    }

    #[test]
    fn a_password_in_a_statuses_text_is_hidden_from_every_factory_item() {
        let secret = "git fetch https://tom:hunter2@github.com/o/foundry.git failed";
        let mut world = World::new();
        let project = world.factory_project(true);
        let mut status = holding(project, 3, 7);
        status.idle = Some(secret.into());
        status.held_up.push(held_up(12, HeldUpKind::Stuck, secret));
        status
            .held_up
            .push(held_up(14, HeldUpKind::NeedsInfo, secret));
        status.foreign.push(ForeignClaim {
            issue: 21,
            title: secret.into(),
            state: "building".into(),
        });
        world.status(world.peer, status);
        let items = factory_items_of(&world);
        assert_eq!(items.len(), 4, "{items:?}");
        for item in &items {
            let text = serde_json::to_string(item).unwrap();
            assert!(!text.contains("hunter2"), "{text}");
            assert!(item.what.contains("***@github.com"), "{}", item.what);
        }
    }

    #[test]
    fn a_machine_that_keeps_no_freshness_raises_factory_idle_only_for_a_reason() {
        let mut world = World::new();
        let project = world.factory_project(true);
        assert!(world.of(Kind::FactoryIdle).is_empty());
        world.status(world.peer, holding(project, 1, 1));
        assert!(world.of(Kind::FactoryIdle).is_empty());
    }

    // foreign claim

    #[test]
    fn a_foreign_claim_in_the_status_raises_foreign_claim() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let mut status = holding(project, 3, 7);
        status.foreign.push(ForeignClaim {
            issue: 21,
            title: "port the gate".into(),
            state: "building".into(),
        });
        world.status(world.peer, status);
        let items = world.of(Kind::ForeignClaim);
        assert_eq!(items.len(), 1, "{items:?}");
        let item = &items[0];
        assert_eq!(item.id, format!("foreign_claim:{project}__21"));
        assert_eq!(item.machine, world.peer);
        assert!(item.what.contains("#21"), "{}", item.what);
        assert!(item.what.contains("port the gate"), "{}", item.what);
        assert!(item.what.contains("factory:building"), "{}", item.what);
    }

    #[test]
    fn a_foreign_claim_the_status_no_longer_lists_clears() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let mut status = holding(project, 3, 7);
        status.foreign.push(ForeignClaim {
            issue: 21,
            title: "port the gate".into(),
            state: "triage".into(),
        });
        world.status(world.peer, status);
        assert_eq!(world.of(Kind::ForeignClaim).len(), 1);
        world.status(world.peer, holding(project, 3, 8));
        assert!(world.of(Kind::ForeignClaim).is_empty());
    }

    // gate infra

    #[test]
    fn a_gate_that_could_not_run_raises_gate_infra() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let mut status = holding(project, 3, 7);
        status.gate_infra.push(run(30));
        world.status(world.peer, status);
        let items = world.of(Kind::GateInfra);
        assert_eq!(items.len(), 1, "{items:?}");
        let item = &items[0];
        assert_eq!(item.id, format!("gate_infra:{}", run(30)));
        assert_eq!(item.machine, world.peer);
        assert!(item.what.contains("o/foundry#30"), "{}", item.what);
    }

    #[test]
    fn a_gate_that_starts_or_a_run_that_ends_clears_gate_infra() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let mut status = holding(project, 3, 7);
        status.gate_infra.push(run(30));
        world.status(world.peer, status);
        assert_eq!(world.of(Kind::GateInfra).len(), 1);
        world.status(world.peer, holding(project, 3, 8));
        assert!(world.of(Kind::GateInfra).is_empty());
    }

    // which status is read

    /// A status that holds the lease and has one of each item a status raises.
    fn busy_status(project: ProjectId, generation: u64) -> FactoryStatus {
        let mut status = holding(project, generation, 7);
        status.idle = Some("gh failed three passes running".into());
        status.held_up.push(held_up(12, HeldUpKind::Stuck, "no"));
        status
            .held_up
            .push(held_up(14, HeldUpKind::NeedsInfo, "which login"));
        status.gate_infra.push(run(30));
        status.foreign.push(ForeignClaim {
            issue: 21,
            title: "port the gate".into(),
            state: "building".into(),
        });
        status
    }

    const FACTORY_KINDS: [Kind; 5] = [
        Kind::RunStuck,
        Kind::RunNeedsInfo,
        Kind::FactoryIdle,
        Kind::ForeignClaim,
        Kind::GateInfra,
    ];

    fn factory_items_of(world: &World) -> Vec<Item> {
        world
            .items()
            .into_iter()
            .filter(|i| FACTORY_KINDS.contains(&i.kind))
            .collect()
    }

    #[test]
    fn a_project_whose_factory_is_off_raises_no_factory_item() {
        let mut world = World::new();
        let project = world.factory_project(false);
        world.status(world.peer, busy_status(project, 3));
        assert!(factory_items_of(&world).is_empty(), "{:?}", world.items());
    }

    #[test]
    fn a_status_from_a_holder_that_is_offline_raises_nothing() {
        let mut world = World::new();
        let project = world.factory_project(true);
        world.status(world.peer, busy_status(project, 3));
        assert_eq!(factory_items_of(&world).len(), 5, "{:?}", world.items());
        world.online.remove(&world.peer);
        assert!(factory_items_of(&world).is_empty(), "{:?}", world.items());
    }

    #[test]
    fn the_holder_at_the_highest_generation_is_believed() {
        let mut world = World::new();
        let project = world.factory_project(true);
        world.status(world.me, busy_status(project, 2));
        world.status(world.peer, holding(project, 3, 1));
        assert!(factory_items_of(&world).is_empty(), "{:?}", world.items());
    }

    // P1's items for factory work (G13)

    /// Names `workspaces` and `sessions` in a live run of a holding status the peer writes.
    fn name_in_status(
        world: &mut World,
        project: ProjectId,
        workspaces: Vec<WorkspaceId>,
        sessions: Vec<SessionId>,
    ) {
        let status = naming(project, 3, world.peer, workspaces, sessions);
        world.status(world.peer, status);
    }

    /// A status that holds `project`'s lease at `generation` and names `workspaces` and
    /// `sessions` in a live run built on `machine`.
    fn naming(
        project: ProjectId,
        generation: u64,
        machine: MachineId,
        workspaces: Vec<WorkspaceId>,
        sessions: Vec<SessionId>,
    ) -> FactoryStatus {
        let mut status = holding(project, generation, 7);
        status.runs.push(LiveRun {
            run: run(12),
            title: "fix login".into(),
            step: "building".into(),
            machine,
            gate_retries_used: 0,
            minutes_used: 4,
            workspaces,
            sessions,
        });
        status
    }

    #[test]
    fn a_factory_session_that_p1_marks_stuck_raises_nothing_and_a_p1_session_still_does() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let peer = world.peer;
        let factory = world.session(peer, SessionState::Stuck);
        let p1 = world.session(peer, SessionState::Stuck);
        name_in_status(&mut world, project, Vec::new(), vec![factory.id]);
        let items = world.of(Kind::SessionStuck);
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].id, format!("session_stuck:{}", p1.id));
    }

    #[test]
    fn a_session_or_setup_a_status_names_raises_no_p1_item() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let peer = world.peer;
        let waiting = world.session(peer, SessionState::WaitingForYou);
        let failed = world.session(peer, SessionState::Failed);
        let (setup_workspace, _) = setup_exited(&mut world, Some(1));
        assert_eq!(world.items().len(), 3, "{:?}", world.items());
        name_in_status(
            &mut world,
            project,
            vec![setup_workspace],
            vec![waiting.id, failed.id],
        );
        assert!(world.items().is_empty(), "{:?}", world.items());
    }

    #[test]
    fn a_p1_session_in_a_factory_workspace_still_raises_its_item() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let (me, peer) = (world.me, world.peer);
        // A person's session in a stuck run's workspace, which a status names.
        let named = world.session(peer, SessionState::WaitingForYou);
        name_in_status(&mut world, project, vec![named.workspace], Vec::new());
        // And one in a workspace this machine marks as factory.
        let marked = world.session(me, SessionState::Stuck);
        world.marked.workspaces.insert(marked.workspace, project);
        let mut kinds: Vec<(Kind, String)> =
            world.items().into_iter().map(|i| (i.kind, i.id)).collect();
        kinds.sort();
        assert_eq!(
            kinds,
            [
                (Kind::SessionStuck, format!("session_stuck:{}", marked.id)),
                (
                    Kind::SessionWaiting,
                    format!("session_waiting:{}", named.id)
                ),
            ]
        );
    }

    #[test]
    fn a_status_of_a_holder_that_is_offline_still_names_factory_work() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let me = world.me;
        let session = world.session(me, SessionState::Stuck);
        name_in_status(&mut world, project, Vec::new(), vec![session.id]);
        world.online.remove(&world.peer);
        // The acting coordinator's status is believed now, and names no run.
        let status = not_holding(
            project,
            NotHolding::HeldBy {
                machine: world.peer,
                generation: 3,
            },
        );
        world.status(me, status);
        assert!(
            world.of(Kind::SessionStuck).is_empty(),
            "{:?}",
            world.items()
        );
    }

    #[test]
    fn a_superseded_holder_that_stays_online_names_no_factory_work() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let (me, peer) = (world.me, world.peer);
        // The peer held the lease at generation 2 with a run, then stopped writing. This
        // machine took it at generation 3, and its status names no run.
        let session = world.session(peer, SessionState::Stuck);
        let (setup_workspace, _) = setup_exited(&mut world, Some(1));
        let old = naming(project, 2, peer, vec![setup_workspace], vec![session.id]);
        world.status(peer, old);
        world.status(me, holding(project, 3, 1));
        let mut kinds: Vec<Kind> = world.items().into_iter().map(|i| i.kind).collect();
        kinds.sort();
        assert_eq!(
            kinds,
            [Kind::SessionStuck, Kind::SetupFailed],
            "{:?}",
            world.items()
        );
    }

    #[test]
    fn an_offline_holder_names_its_runs_until_a_newer_holder_takes_over() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let (me, peer) = (world.me, world.peer);
        let session = world.session(me, SessionState::Stuck);
        world.status(peer, naming(project, 3, me, Vec::new(), vec![session.id]));
        world.online.remove(&peer);
        // This machine acts, and sees the lease still held by the peer at generation 3.
        let waiting = not_holding(
            project,
            NotHolding::HeldBy {
                machine: peer,
                generation: 3,
            },
        );
        world.status(me, waiting);
        assert!(
            world.of(Kind::SessionStuck).is_empty(),
            "{:?}",
            world.items()
        );
        // This machine takes the lease at generation 4, and its status names no run.
        world.status(me, holding(project, 4, 1));
        assert_eq!(world.of(Kind::SessionStuck).len(), 1, "{:?}", world.items());
    }

    #[test]
    fn a_believed_status_below_the_top_generation_names_no_factory_work() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let peer = world.peer;
        // The peer held at generation 2 and stalled while online. A newer holder took the
        // lease at generation 3 and went offline, so the peer's stale status is believed. It
        // was superseded all the same, so the sessions it names raise P1's items again.
        let session = world.session(peer, SessionState::Stuck);
        world.status(peer, naming(project, 2, peer, Vec::new(), vec![session.id]));
        world.status(MachineId::random(), holding(project, 3, 1));
        assert_eq!(world.of(Kind::SessionStuck).len(), 1, "{:?}", world.items());
    }

    #[test]
    fn a_newer_holder_the_acting_coordinator_sees_ends_an_offline_holders_cover() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let (me, peer) = (world.me, world.peer);
        let session = world.session(me, SessionState::Stuck);
        world.status(peer, naming(project, 3, me, Vec::new(), vec![session.id]));
        world.online.remove(&peer);
        let held_by = |machine, generation| {
            not_holding(
                project,
                NotHolding::HeldBy {
                    machine,
                    generation,
                },
            )
        };
        // The lease ref still names the offline peer at generation 3: its runs stay covered.
        world.status(me, held_by(peer, 3));
        assert!(
            world.of(Kind::SessionStuck).is_empty(),
            "{:?}",
            world.items()
        );
        // Another machine took it at generation 4, and its status has not arrived yet.
        world.status(me, held_by(MachineId::random(), 4));
        assert_eq!(world.of(Kind::SessionStuck).len(), 1, "{:?}", world.items());
    }

    #[test]
    fn a_status_of_a_project_whose_factory_is_off_names_no_factory_work() {
        let mut world = World::new();
        let project = world.factory_project(false);
        let peer = world.peer;
        let session = world.session(peer, SessionState::Stuck);
        name_in_status(&mut world, project, Vec::new(), vec![session.id]);
        assert_eq!(world.of(Kind::SessionStuck).len(), 1, "{:?}", world.items());
    }

    #[test]
    fn sessions_and_workspaces_this_machine_marks_as_factory_raise_no_p1_item() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let me = world.me;
        let stuck = world.session(me, SessionState::Stuck);
        let waiting = world.session(me, SessionState::WaitingForYou);
        let setup = TerminalId::random();
        let workspace = world.workspace(me, Some(setup));
        world.terminal(me, setup, workspace, Some(Some(1)), false);
        assert_eq!(world.items().len(), 3, "{:?}", world.items());

        world.marked.sessions.insert(stuck.id, project);
        world.marked.sessions.insert(waiting.id, project);
        world.marked.workspaces.insert(workspace, project);
        assert!(world.items().is_empty(), "{:?}", world.items());
    }

    #[test]
    fn factory_work_this_machine_marks_for_a_project_whose_factory_is_off_still_raises_p1_items() {
        let mut world = World::new();
        let project = world.factory_project(false);
        let me = world.me;
        let stuck = world.session(me, SessionState::Stuck);
        let setup = TerminalId::random();
        let workspace = world.workspace(me, Some(setup));
        world.terminal(me, setup, workspace, Some(Some(1)), false);
        world.marked.sessions.insert(stuck.id, project);
        world.marked.workspaces.insert(workspace, project);
        assert_eq!(world.items().len(), 2, "{:?}", world.items());
    }

    #[test]
    fn the_setup_of_a_workspace_with_a_marked_factory_session_raises_nothing() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let me = world.me;
        let setup = TerminalId::random();
        let workspace = world.workspace(me, Some(setup));
        world.terminal(me, setup, workspace, Some(Some(1)), false);
        let session = Session {
            id: SessionId::random(),
            workspace,
            terminal: TerminalId::random(),
            model: "opus".into(),
            state: SessionState::Working,
            name: None,
        };
        world.put_session(me, session.clone(), false);
        assert_eq!(world.of(Kind::SetupFailed).len(), 1);
        world.marked.sessions.insert(session.id, project);
        assert!(world.items().is_empty(), "{:?}", world.items());
    }

    #[test]
    fn this_machines_marks_say_nothing_of_a_peers_records() {
        let mut world = World::new();
        let project = world.factory_project(true);
        let peer = world.peer;
        let session = world.session(peer, SessionState::Stuck);
        world.marked.sessions.insert(session.id, project);
        world.marked.workspaces.insert(session.workspace, project);
        assert_eq!(world.of(Kind::SessionStuck).len(), 1, "{:?}", world.items());
    }

    #[test]
    fn marks_and_statuses_count_only_for_their_own_projects_factory() {
        let mut world = World::new();
        let on = world.factory_project(true);
        let off = world.factory_project(false);
        let (me, peer) = (world.me, world.peer);
        let failed_setup = |world: &mut World| {
            let setup = TerminalId::random();
            let workspace = world.workspace(me, Some(setup));
            world.terminal(me, setup, workspace, Some(Some(1)), false);
            workspace
        };
        // This machine marks a stuck session and a failed setup of each project.
        let (marked_on, marked_off) = (
            world.session(me, SessionState::Stuck),
            world.session(me, SessionState::Stuck),
        );
        let (setup_on, setup_off) = (failed_setup(&mut world), failed_setup(&mut world));
        world.marked.sessions.insert(marked_on.id, on);
        world.marked.sessions.insert(marked_off.id, off);
        world.marked.workspaces.insert(setup_on, on);
        world.marked.workspaces.insert(setup_off, off);
        // Each project's status names a waiting session of the peer.
        let (named_on, named_off) = (
            world.session(peer, SessionState::WaitingForYou),
            world.session(peer, SessionState::WaitingForYou),
        );
        name_in_status(&mut world, on, Vec::new(), vec![named_on.id]);
        name_in_status(&mut world, off, Vec::new(), vec![named_off.id]);

        let mut p1: Vec<String> = world
            .items()
            .into_iter()
            .filter(|i| {
                [Kind::SessionStuck, Kind::SessionWaiting, Kind::SetupFailed].contains(&i.kind)
            })
            .map(|i| i.id)
            .collect();
        p1.sort();
        let mut want = vec![
            format!("session_stuck:{}", marked_off.id),
            format!("setup_failed:{setup_off}"),
            format!("session_waiting:{}", named_off.id),
        ];
        want.sort();
        assert_eq!(p1, want, "{:?}", world.items());
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
                factory_versions: BTreeMap::new(),
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

        /// Opens a link from `peer` that stays open, is heard from, and answers every edit it is
        /// sent with `outcome`, as an acting coordinator on this build would.
        async fn answering(&self, peer: MachineId, outcome: Outcome) {
            let (ours, theirs) = Conn::pair();
            tokio::spawn({
                let peers = self.peers.clone();
                async move { peers.accept(ours).await }
            });
            let Conn { send, mut recv } = theirs;
            let hello = PeerMessage::Hello {
                protocol: PROTOCOL,
                machine: peer,
                versions: BTreeMap::new(),
                factory_versions: BTreeMap::new(),
            };
            let nothing = PeerMessage::Records {
                protocol: PROTOCOL,
                records: Vec::new(),
            };
            for message in [hello, nothing] {
                send.send(serde_json::to_string(&message).unwrap())
                    .await
                    .unwrap();
            }
            tokio::spawn(async move {
                while let Some(text) = recv.recv().await {
                    let Ok(PeerMessage::Edit { id: Some(id), .. }) = serde_json::from_str(&text)
                    else {
                        continue;
                    };
                    let answer = PeerMessage::Answer {
                        protocol: PROTOCOL,
                        id,
                        outcome: outcome.clone(),
                    };
                    let text = serde_json::to_string(&answer).unwrap();
                    if send.send(text).await.is_err() {
                        return;
                    }
                }
            });
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while !self.peers.online().contains(&peer) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the peer was not heard"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
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

    /// Settings with a project named `foundry` whose factory is on, and this machine first in
    /// the coordinator order.
    fn with_factory_on(me: MachineId, settings: &mut SharedSettings) -> ProjectId {
        let project = Project {
            id: ProjectId::random(),
            name: "foundry".into(),
            repo: "https://github.com/o/foundry.git".into(),
        };
        let id = project.id;
        settings.projects.push(project);
        settings.project_factory.insert(
            id,
            ProjectFactory {
                on: true,
                login: "octocat".into(),
                paused: false,
            },
        );
        settings.priority = vec![me];
        id
    }

    #[tokio::test]
    async fn the_list_raises_the_factory_items_of_the_statuses_the_store_holds() {
        let project = Arc::new(Mutex::new(None));
        let served = Served::new({
            let project = project.clone();
            move |me, settings| *project.lock().unwrap() = Some(with_factory_on(me, settings))
        })
        .await;
        let project = project.lock().unwrap().unwrap();
        let mut status = holding(project, 1, 1);
        status.held_up.push(held_up(12, HeldUpKind::Stuck, "no"));
        let session = SessionId::random();
        status.runs.push(LiveRun {
            run: run(13),
            title: "fix login".into(),
            step: "building".into(),
            machine: served.me,
            gate_retries_used: 0,
            minutes_used: 1,
            workspaces: Vec::new(),
            sessions: vec![session],
        });
        let record = FactoryRecord {
            owner: served.me,
            version: 1,
            body: FactoryBody::Status(status),
        };
        // The session the status names is stuck as P1 sees it.
        let stuck = AnyRecord::Session(Record {
            owner: served.me,
            version: 1,
            deleted: false,
            body: Session {
                id: session,
                workspace: WorkspaceId::random(),
                terminal: TerminalId::random(),
                model: "opus".into(),
                state: SessionState::Stuck,
                name: None,
            },
        });
        {
            let mut store = served.store.lock().unwrap();
            store.apply_factory(&record, served.me).unwrap();
            store.apply(&stuck, served.me).unwrap();
        }
        let kinds = served.kinds().await;
        let me = served.me.to_string();
        assert!(
            kinds.contains(&("run_stuck".to_owned(), me.clone())),
            "{kinds:?}"
        );
        assert!(
            !kinds.contains(&("session_stuck".to_owned(), me)),
            "{kinds:?}"
        );
    }

    #[tokio::test]
    async fn freshness_and_marks_given_to_the_list_are_read() {
        let data = tempfile::tempdir().unwrap();
        let me = MachineId::random();
        let mut store = Store::open(data.path().join("foundry.db")).unwrap();
        let mut held = store.settings().unwrap();
        held.seq += 1;
        let project = with_factory_on(me, &mut held);
        store.apply_settings(held).unwrap();
        // A stuck session of this machine that it marks as factory.
        let session = Session {
            id: SessionId::random(),
            workspace: WorkspaceId::random(),
            terminal: TerminalId::random(),
            model: "opus".into(),
            state: SessionState::Stuck,
            name: None,
        };
        let record = AnyRecord::Session(Record {
            owner: me,
            version: 1,
            deleted: false,
            body: session.clone(),
        });
        store.apply(&record, me).unwrap();
        let mut marked = Marked::default();
        marked.sessions.insert(session.id, project);
        let store = Arc::new(Mutex::new(store));
        let peers = Peers::new(me, store.clone(), Arc::new(NoDial));
        let coordinator = Arc::new(Coordinator::new(me, store.clone(), peers.clone()));
        let clock = Arc::new(FakeClock::new());
        let dyn_clock: Arc<dyn Clock> = clock.clone();
        let freshness = Arc::new(Freshness::new(dyn_clock.clone()));
        let attention = Attention::new(me, store.clone(), peers, coordinator).with_factory(
            dyn_clock,
            freshness.clone(),
            Arc::new(marked),
        );
        let kinds = |items: Vec<Item>| items.into_iter().map(|i| i.kind).collect::<Vec<_>>();

        assert_eq!(kinds(attention.items().await.unwrap()), []);
        clock.advance(10 * MINUTE);
        assert_eq!(kinds(attention.items().await.unwrap()), [Kind::FactoryIdle]);
        freshness.held(project);
        assert_eq!(kinds(attention.items().await.unwrap()), []);
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

    #[test]
    fn a_read_takes_in_only_the_records_written_since_the_last_read() {
        let mut world = World::new();
        busy_peer(&mut world);
        let (me, peer) = (world.me, world.peer);
        let waiting = world.session(me, SessionState::WaitingForYou);
        let data = tempfile::tempdir().unwrap();
        let mut store = Store::open(data.path().join("foundry.db")).unwrap();
        for record in &world.records {
            store.apply(record, record.owner()).unwrap();
        }
        let mut held = Held::default();
        assert_eq!(held.load(&store).unwrap(), world.records.len());
        assert_eq!(held.load(&store).unwrap(), 0);

        // One record changes: the next read takes in that one, in place of the copy held.
        world.put_session(
            me,
            Session {
                state: SessionState::Working,
                ..waiting.clone()
            },
            false,
        );
        let changed = world.records.last().unwrap();
        store.apply(changed, me).unwrap();
        assert_eq!(held.load(&store).unwrap(), 1);
        assert_eq!(held.load(&store).unwrap(), 0);
        let mut seen = held.as_seen(&[me, peer].into());
        let by_version = |records: &mut Vec<AnyRecord>| records.sort_by_key(AnyRecord::version);
        by_version(&mut seen);
        by_version(&mut world.records);
        assert_eq!(seen, world.records);
    }

    #[tokio::test]
    async fn the_list_follows_a_record_from_one_read_to_the_next() {
        let mut world = World::new();
        let (peer, machines) = (world.peer, world.settings.machines.clone());
        let waiting = world.session(peer, SessionState::WaitingForYou);
        let served = Served::new(|_, settings| settings.machines = machines).await;
        served.hold(&world);
        served.hear(peer).await;
        let raised = ("session_waiting".to_owned(), peer.to_string());
        assert!(served.kinds().await.contains(&raised));

        // It is answered, then waits again: each read shows the session as it is by then.
        for (state, stands) in [
            (SessionState::Working, false),
            (SessionState::WaitingForYou, true),
        ] {
            let session = Session {
                state,
                ..waiting.clone()
            };
            world.put_session(peer, session, false);
            served.hold(&world);
            let kinds = served.kinds().await;
            assert_eq!(kinds.contains(&raised), stands, "{state:?}: {kinds:?}");
        }
    }

    #[tokio::test]
    async fn a_record_this_build_cannot_read_hides_its_owners_items_and_no_other_owners() {
        let mut world = World::new();
        let (other, peer, machines) = (world.me, world.peer, world.settings.machines.clone());
        world.sample(other, 100 * GB, Some("v0.2.0"));
        world.sample(peer, 100 * GB, Some("v0.2.0"));
        let served = Served::new(|_, settings| settings.machines = machines).await;
        served.hold(&world);
        let refused = |machine: MachineId| ("update_refused".to_owned(), machine.to_string());
        let kinds = served.kinds().await;
        assert!(kinds.contains(&refused(other)), "{kinds:?}");
        assert!(kinds.contains(&refused(peer)), "{kinds:?}");

        // A newer build on the peer left a row before a rollback: the peer's records, the
        // ones already read too, are left out as the state leaves them out.
        let conn = rusqlite::Connection::open(served._data.path().join("foundry.db")).unwrap();
        conn.execute(
            "INSERT INTO records (kind, id, owner, version, record) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "printer",
                "unreadable",
                peer.to_string(),
                1000,
                r#"{"kind":"printer"}"#
            ],
        )
        .unwrap();
        let kinds = served.kinds().await;
        assert!(kinds.contains(&refused(other)), "{kinds:?}");
        assert!(!kinds.contains(&refused(peer)), "{kinds:?}");
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

    #[test]
    fn only_an_edit_that_may_have_been_numbered_is_504_and_a_refusal_is_502() {
        let id = MachineId::random();
        let cases = [
            (CoordinatorError::NotLinked(id), 503),
            (
                CoordinatorError::Older {
                    acting: id,
                    speaks: 3,
                },
                503,
            ),
            (
                CoordinatorError::Unsettled {
                    from: id,
                    acting: id,
                },
                503,
            ),
            (
                CoordinatorError::Refused {
                    reason: "no".into(),
                },
                502,
            ),
            (CoordinatorError::NoAnswer { acting: id }, 504),
            (
                CoordinatorError::Unwritable(serde_json::from_str::<u8>("x").unwrap_err()),
                500,
            ),
            (
                CoordinatorError::Store(StoreError::TooLarge {
                    what: "x",
                    value: 1,
                }),
                500,
            ),
        ];
        for (error, status) in cases {
            assert_eq!(edit_status(&error).as_u16(), status, "{error}");
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
                key: String::new(),
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

    #[tokio::test]
    async fn a_dismissal_the_acting_coordinator_refuses_answers_the_refusal_without_the_value() {
        let peer = MachineId::random();
        let served = Served::new(|me, settings| {
            with_overwritten(me, settings);
            settings.machines = vec![Machine {
                id: peer,
                name: "trilogy-mac".into(),
                address: "100.64.0.2".into(),
                key: String::new(),
            }];
            settings.priority = vec![peer, me];
        })
        .await;
        // The peer acts, and refuses the dismissal it is sent.
        let reason = format!("{peer} could not store it");
        served
            .answering(
                peer,
                Outcome::Refused {
                    reason: reason.clone(),
                    conflict: false,
                },
            )
            .await;

        let key = served.overwritten().into_keys().next().unwrap();
        let (status, body) = served
            .call(
                "POST",
                &format!("/api/attention/setting_overwritten:{key}/dismiss"),
            )
            .await;
        assert_eq!(status, 502, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let error = json["error"].as_str().unwrap();
        assert!(error.contains(&reason), "{error}");
        for value in ["hunter2", "github.com", "secret-name", "other-name"] {
            assert!(!body.contains(value), "{value} in {body}");
        }
        assert_eq!(served.overwritten().len(), 1);
    }

    #[tokio::test]
    async fn a_dismissal_the_acting_coordinator_numbers_answers_204() {
        let peer = MachineId::random();
        let served = Served::new(|me, settings| {
            with_overwritten(me, settings);
            settings.machines = vec![Machine {
                id: peer,
                name: "trilogy-mac".into(),
                address: "100.64.0.2".into(),
                key: String::new(),
            }];
            settings.priority = vec![peer, me];
        })
        .await;
        served
            .answering(
                peer,
                Outcome::Numbered {
                    stamp: Stamp { seq: 99, by: peer },
                },
            )
            .await;

        let key = served.overwritten().into_keys().next().unwrap();
        let (status, body) = served
            .call(
                "POST",
                &format!("/api/attention/setting_overwritten:{key}/dismiss"),
            )
            .await;
        assert_eq!(status, 204, "{body}");
    }
}
