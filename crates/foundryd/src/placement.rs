//! Picks a machine for a workspace, or gives the reasons none fits, and holds the requests that
//! wait for one.
//!
//! [`place`] is pure. Among the online machines under every limit they have, it picks the one
//! with the lowest load per core, fewer sessions breaking a tie. A machine with no limits in
//! the shared settings has none to be over. When none qualifies it gives one [`Reason`] per
//! machine, such as `trilogy-mac: load 1.8 over limit 1.0` or `old-mini: offline`.
//!
//! A create with `"machine": "choose"` (see [`crate::api`]) that finds no machine waits in this
//! machine's [`Queue`]. The queue is in memory and not synced, so only this machine's UI lists
//! it, and a daemon that stops drops and logs each request. [`run`] tries the waiting requests
//! again every [`RETRY_EVERY`], so a request is placed within a second of a sample that shows a
//! machine qualifies. A placed request is made the way the UI makes one: the create on the
//! picked machine, carried there when it is another, then one session with the request's model
//! and first prompt.
//!
//! - `GET /api/placements`: `{"requests": [...]}`, each a [`Listed`].
//! - `POST /api/placements/{id}/cancel`: `204`, and the request is never placed. One being
//!   placed now is `409`; an id this machine holds no request for is `404`.
//!
//! A machine just sent a create takes no other until its next sample, which counts the new
//! session; one that cannot take the call (offline, older, its link down) is passed over the
//! same way, and the next roomiest is tried.
//!
//! A placement whose create or session start fails stays listed with its error and is not tried
//! again; cancelling it dismisses it. A request held longer than [`WAITING_ALERT_AFTER`], waiting
//! or failed, raises a "placement waiting" attention item, through this queue's [`Source`]. The
//! item clears once the request is placed or cancelled.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use foundry_proto::ids::{MachineId, ProjectId, WorkspaceId};
use foundry_proto::record::{AnyRecord, MachineSample, Record};
use foundry_proto::settings::{Limits, SharedSettings};
use serde::Serialize;
use tokio::time::{Instant, MissedTickBehavior};
use uuid::Uuid;

use crate::api::{self, Api};
use crate::attention::{Fleet, Item, Kind, Source};
use crate::machine::{self, Over, over_limits};
use crate::store::{Store, StoreError};
use crate::sync::call::CallError;
use crate::sync::{lock, on_store};

/// How often the waiting requests are tried again. A machine samples every 5 seconds, so a
/// request is placed within 6 seconds of a machine qualifying.
pub const RETRY_EVERY: Duration = Duration::from_secs(1);

/// How long a request waits before it raises a "placement waiting" item.
pub const WAITING_ALERT_AFTER: Duration = Duration::from_secs(5 * 60);

/// Why one machine was not picked.
#[derive(Clone, Debug, PartialEq)]
pub struct Reason {
    pub machine: MachineId,
    pub why: Why,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Why {
    Offline,
    /// Every limit its newest sample is at or over, in the order sessions, load, disk.
    Over(Vec<Over>),
}

impl Reason {
    /// The reason as the UI shows it, such as `vps: sessions 4 at limit 4, load 1.5 over limit
    /// 1.0`.
    pub fn text(&self, name: &str) -> String {
        format!("{name}: {}", self.what(name))
    }

    /// The reason without the machine's name in front.
    fn what(&self, name: &str) -> String {
        match &self.why {
            Why::Offline => "offline".to_owned(),
            Why::Over(over) => {
                let named = format!("{name}: ");
                let each: Vec<String> = over
                    .iter()
                    .map(|over| {
                        let reason = machine::Reason {
                            machine: self.machine,
                            over: over.clone(),
                        };
                        let text = reason.text(name);
                        text.strip_prefix(&named)
                            .map_or(text.clone(), str::to_owned)
                    })
                    .collect();
                each.join(", ")
            }
        }
    }
}

/// The machine a new workspace goes to: among the machines `samples` come from that are
/// `online` and under every limit `limits` gives them, the one with the lowest load per core,
/// then the fewest sessions. When none qualifies, one [`Reason`] per machine, in the order of
/// `samples`.
pub fn place(
    samples: &[Record<MachineSample>],
    limits: &BTreeMap<MachineId, Limits>,
    online: &BTreeSet<MachineId>,
) -> Result<MachineId, Vec<Reason>> {
    let mut reasons = Vec::new();
    let mut best: Option<&Record<MachineSample>> = None;
    for sample in samples {
        let machine = sample.owner;
        if !online.contains(&machine) {
            reasons.push(Reason {
                machine,
                why: Why::Offline,
            });
            continue;
        }
        let over: Vec<Over> = limits
            .get(&machine)
            .map(|limits| over_limits(sample, limits))
            .unwrap_or_default()
            .into_iter()
            .map(|reason| reason.over)
            .collect();
        if !over.is_empty() {
            reasons.push(Reason {
                machine,
                why: Why::Over(over),
            });
        } else if best.is_none_or(|best| roomier(sample, best)) {
            best = Some(sample);
        }
    }
    best.map(|best| best.owner).ok_or(reasons)
}

/// Whether `a` has more headroom than `b`: less load per core, then fewer sessions, then the
/// lower id, so the pick never depends on the order of the samples.
fn roomier(a: &Record<MachineSample>, b: &Record<MachineSample>) -> bool {
    a.body
        .load_per_core
        .total_cmp(&b.body.load_per_core)
        .then(a.body.sessions.cmp(&b.body.sessions))
        .then(a.owner.cmp(&b.owner))
        .is_lt()
}

/// What a waiting request makes once it is placed.
#[derive(Clone, Debug)]
pub struct Want {
    pub project: ProjectId,
    pub base: String,
    pub name: String,
    /// The model of the one session started in it.
    pub model: String,
    /// That session's first prompt.
    pub prompt: Option<String>,
}

/// How a placement went.
#[derive(Debug)]
pub enum Outcome {
    /// The workspace was made and its session started.
    Placed,
    /// No machine took it: the request waits again, with new reasons when there are any.
    Wait(Option<Vec<String>>),
    /// The create or the session start failed, saying why. The request stays listed with it,
    /// and is not tried again.
    Failed(String),
}

/// Why a request cannot be cancelled.
#[derive(Debug, PartialEq)]
pub enum Refused {
    /// This machine holds no request with the id.
    Unknown,
    /// It is being placed now.
    Placing,
}

/// A request as `GET /api/placements` lists it.
#[derive(Clone, Debug, Serialize)]
pub struct Listed {
    pub id: Uuid,
    pub project: ProjectId,
    pub name: String,
    pub state: Phase,
    /// When it started waiting, in milliseconds since the epoch, for display only.
    pub waiting_since: u64,
    /// Why no machine took it, as of the last try.
    pub reasons: Vec<String>,
    /// Why its placement failed.
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Waiting,
    Placing,
    Failed,
}

struct Request {
    id: Uuid,
    want: Want,
    started: Instant,
    since: u64,
    reasons: Vec<String>,
    phase: Phase,
    error: Option<String>,
}

/// The newest sample held from each machine, loaded a little at a time: each read takes in only
/// the records written since the read before it.
#[derive(Default)]
struct Samples {
    seen: BTreeMap<MachineId, u64>,
    newest: BTreeMap<MachineId, Record<MachineSample>>,
}

impl Samples {
    fn load(&mut self, store: &Store) -> Result<(), StoreError> {
        for (owner, version) in store.versions()? {
            let from = self.seen.get(&owner).copied().unwrap_or(0);
            if version <= from {
                continue;
            }
            match store.since(owner, from) {
                Ok(records) => {
                    for record in records {
                        if let AnyRecord::MachineSample(sample) = record {
                            if sample.deleted {
                                self.newest.remove(&owner);
                            } else {
                                self.newest.insert(owner, sample);
                            }
                        }
                    }
                }
                // A record this build cannot read, one a newer build wrote before a rollback,
                // is passed over with that owner's other records up to the newest held; the
                // owner's next sample is read as usual.
                Err(error @ StoreError::Json(_)) => {
                    tracing::warn!("placement: records of {owner} not read: {error}");
                }
                Err(error) => return Err(error),
            }
            self.seen.insert(owner, version);
        }
        Ok(())
    }
}

/// A machine left out of picks until a sample newer than `version` arrives: it was just given
/// a create its sample does not count yet, or it could not take the call, saying `why`.
struct Bench {
    version: u64,
    why: Option<String>,
}

/// What a pick reads: each machine's newest sample, the settings, who is online, and the
/// machines left out until their next sample.
struct View {
    samples: Vec<Record<MachineSample>>,
    settings: SharedSettings,
    online: BTreeSet<MachineId>,
    /// Each benched machine and why, as a reason's text after its name.
    benched: BTreeMap<MachineId, String>,
}

impl View {
    fn name(&self, machine: MachineId) -> String {
        self.settings
            .machines
            .iter()
            .find(|m| m.id == machine)
            .map_or_else(|| machine.to_string(), |m| m.name.clone())
    }

    /// The samples of the machines a pick may take.
    fn candidates(&self) -> Vec<Record<MachineSample>> {
        self.samples
            .iter()
            .filter(|s| !self.benched.contains_key(&s.owner))
            .cloned()
            .collect()
    }

    fn version(&self, machine: MachineId) -> u64 {
        self.samples
            .iter()
            .find(|s| s.owner == machine)
            .map_or(0, |s| s.version)
    }

    /// Each reason, then each benched machine's, as the UI shows them.
    fn texts(&self, reasons: &[Reason]) -> Vec<String> {
        let benched = self
            .benched
            .iter()
            .map(|(machine, why)| format!("{}: {why}", self.name(*machine)));
        reasons
            .iter()
            .map(|reason| reason.text(&self.name(reason.machine)))
            .chain(benched)
            .collect()
    }
}

/// A create sent to the machine picked for it, and what that machine answered.
pub(crate) struct Sent {
    pub machine: MachineId,
    /// The machine's name, for messages.
    pub name: String,
    pub answer: Result<(StatusCode, serde_json::Value), CallError>,
}

/// What a reason says of a machine that refused a call before it was sent, or `None` when the
/// call may have run there.
fn unreachable(refused: &CallError) -> Option<String> {
    match refused {
        CallError::Lost(_) => None,
        CallError::NotListed(_) => Some("not in the machine list".to_owned()),
        CallError::Offline(_) => Some("offline".to_owned()),
        CallError::Older(_) => Some("runs an older Foundry, which cannot take a create".to_owned()),
        CallError::Down(_) => Some("its link is down".to_owned()),
        CallError::Refused(reason) => Some(format!("refused the call: {reason}")),
    }
}

/// This machine's requests waiting for a machine, in the order they came.
pub struct Queue {
    me: MachineId,
    requests: Mutex<Vec<Request>>,
    samples: Arc<Mutex<Samples>>,
    benched: Mutex<BTreeMap<MachineId, Bench>>,
}

impl Queue {
    pub fn new(me: MachineId) -> Self {
        Self {
            me,
            requests: Mutex::default(),
            samples: Arc::default(),
            benched: Mutex::default(),
        }
    }

    /// Sends a create of `name` from `base` in `project` to the roomiest machine that takes the
    /// call, or gives the reasons no machine qualifies, as the UI shows them. A machine that
    /// refuses the call before it is sent is benched and the next is tried; the machine sent
    /// the create is benched too, as its sample does not count the new workspace's session yet.
    pub(crate) async fn create(
        &self,
        api: &Api,
        project: ProjectId,
        base: &str,
        name: &str,
    ) -> Result<Result<Sent, Vec<String>>, StoreError> {
        // Each machine is tried once per call, even when a new sample takes it off the bench.
        let mut tried = BTreeSet::new();
        loop {
            let view = self.view(api).await?;
            let mut candidates = view.candidates();
            candidates.retain(|s| !tried.contains(&s.owner));
            let machine = match place(&candidates, &view.settings.limits, &view.online) {
                Ok(machine) => machine,
                Err(reasons) => return Ok(Err(view.texts(&reasons))),
            };
            tried.insert(machine);
            let version = view.version(machine);
            let answer = api::create_on(api, machine, project, base, name).await;
            let why = answer.as_ref().err().and_then(unreachable);
            let refused = why.is_some();
            lock(&self.benched).insert(machine, Bench { version, why });
            // Benched, so the next pass picks another machine or gives the reasons.
            if !refused {
                return Ok(Ok(Sent {
                    machine,
                    name: view.name(machine),
                    answer,
                }));
            }
        }
    }

    /// What to warn of when a person names `machine` for a workspace: each limit its newest
    /// sample is at or over, or `None` when it is under every one.
    pub(crate) async fn warning(
        &self,
        api: &Api,
        machine: MachineId,
    ) -> Result<Option<String>, StoreError> {
        let view = self.view(api).await?;
        let over: Vec<Over> = match (
            view.samples.iter().find(|s| s.owner == machine),
            view.settings.limits.get(&machine),
        ) {
            (Some(sample), Some(limits)) => over_limits(sample, limits)
                .into_iter()
                .map(|r| r.over)
                .collect(),
            _ => Vec::new(),
        };
        if over.is_empty() {
            return Ok(None);
        }
        let name = view.name(machine);
        let reason = Reason {
            machine,
            why: Why::Over(over),
        };
        Ok(Some(format!(
            "{name} is over its limits: {}",
            reason.what(&name)
        )))
    }

    /// Each listed machine's newest sample, in the list's order, and this machine's when it is
    /// not listed; only the records written since the last read are loaded. A benched machine
    /// whose newest sample is newer than its bench is benched no longer.
    async fn view(&self, api: &Api) -> Result<View, StoreError> {
        let online = api.peers.online();
        let samples = Arc::clone(&self.samples);
        let (mut newest, settings) = on_store(&api.store, move |store| {
            let mut samples = lock(&samples);
            samples.load(store)?;
            Ok::<_, StoreError>((samples.newest.clone(), store.settings()?))
        })
        .await?;
        let mut order: Vec<MachineId> = settings.machines.iter().map(|m| m.id).collect();
        if !order.contains(&self.me) {
            order.push(self.me);
        }
        let samples: Vec<Record<MachineSample>> =
            order.iter().filter_map(|id| newest.remove(id)).collect();
        let mut benched = lock(&self.benched);
        benched.retain(|machine, bench| {
            samples
                .iter()
                .find(|s| s.owner == *machine)
                .is_some_and(|s| s.version <= bench.version)
        });
        let benched = benched
            .iter()
            .map(|(machine, bench)| {
                let why = bench
                    .why
                    .clone()
                    .unwrap_or_else(|| "given a workspace since its last sample".to_owned());
                (*machine, why)
            })
            .collect();
        Ok(View {
            samples,
            settings,
            online,
            benched,
        })
    }

    /// Adds a request that no machine takes now, for the `reasons` given, and returns its id.
    pub fn wait(&self, want: Want, reasons: Vec<String>) -> Uuid {
        let id = Uuid::new_v4();
        tracing::info!(
            "placement: {} waits for a machine: {}",
            want.name,
            reasons.join("; ")
        );
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        lock(&self.requests).push(Request {
            id,
            want,
            started: Instant::now(),
            since,
            reasons,
            phase: Phase::Waiting,
            error: None,
        });
        id
    }

    /// Every request held, in the order they came.
    pub fn list(&self) -> Vec<Listed> {
        lock(&self.requests)
            .iter()
            .map(|r| Listed {
                id: r.id,
                project: r.want.project,
                name: r.want.name.clone(),
                state: r.phase,
                waiting_since: r.since,
                reasons: r.reasons.clone(),
                error: r.error.clone(),
            })
            .collect()
    }

    /// Removes request `id` so it is never placed: one waiting, or one whose placement failed.
    pub fn cancel(&self, id: Uuid) -> Result<(), Refused> {
        let mut requests = lock(&self.requests);
        let Some(at) = requests.iter().position(|r| r.id == id) else {
            return Err(Refused::Unknown);
        };
        if requests[at].phase == Phase::Placing {
            return Err(Refused::Placing);
        }
        let request = requests.remove(at);
        tracing::info!(
            "placement: the request for {} is cancelled",
            request.want.name
        );
        Ok(())
    }

    /// Drops every request, logging each, as the daemon stops. Returns how many it dropped.
    pub fn drop_all(&self) -> usize {
        let dropped = std::mem::take(&mut *lock(&self.requests));
        for request in &dropped {
            let name = &request.want.name;
            if request.phase == Phase::Placing {
                tracing::warn!(
                    "placement: the daemon stops while {name} is being placed; its workspace \
                     may exist without its session"
                );
            } else {
                tracing::warn!("placement: the daemon stops, so the request for {name} is dropped");
            }
        }
        dropped.len()
    }

    /// The ids of the requests waiting, in the order they came.
    fn waiting(&self) -> Vec<Uuid> {
        lock(&self.requests)
            .iter()
            .filter(|r| r.phase == Phase::Waiting)
            .map(|r| r.id)
            .collect()
    }

    /// Marks waiting request `id` as being placed and gives what it makes, or `None` when it is
    /// not waiting: cancelled, failed or being placed already.
    pub(crate) fn begin(&self, id: Uuid) -> Option<Want> {
        let mut requests = lock(&self.requests);
        let request = requests
            .iter_mut()
            .find(|r| r.id == id && r.phase == Phase::Waiting)?;
        request.phase = Phase::Placing;
        Some(request.want.clone())
    }

    /// Ends the placement [`begin`](Self::begin) started.
    pub(crate) fn finish(&self, id: Uuid, outcome: Outcome) {
        let mut requests = lock(&self.requests);
        let Some(at) = requests.iter().position(|r| r.id == id) else {
            return;
        };
        match outcome {
            Outcome::Placed => {
                requests.remove(at);
            }
            Outcome::Wait(reasons) => {
                let request = &mut requests[at];
                request.phase = Phase::Waiting;
                if let Some(reasons) = reasons {
                    request.reasons = reasons;
                }
            }
            Outcome::Failed(error) => {
                tracing::warn!("placement: {error}");
                let request = &mut requests[at];
                request.phase = Phase::Failed;
                request.error = Some(error);
            }
        }
    }

    /// Tries each waiting request once, in the order they came.
    async fn retry(&self, api: &Api) -> Result<(), StoreError> {
        for id in self.waiting() {
            let Some(want) = self.begin(id) else {
                continue;
            };
            let sent = match self.create(api, want.project, &want.base, &want.name).await {
                Ok(sent) => sent,
                Err(error) => {
                    self.finish(id, Outcome::Wait(None));
                    return Err(error);
                }
            };
            let outcome = match sent {
                Err(reasons) => Outcome::Wait(Some(reasons)),
                Ok(sent) => {
                    let outcome = start(api, &want, sent).await;
                    if matches!(outcome, Outcome::Placed) {
                        tracing::info!("placement: {} is placed", want.name);
                    }
                    outcome
                }
            };
            self.finish(id, outcome);
        }
        Ok(())
    }
}

/// Starts `want`'s session in the workspace `sent` made, or says why it was not placed.
async fn start(api: &Api, want: &Want, sent: Sent) -> Outcome {
    let Sent {
        machine,
        name,
        answer,
    } = sent;
    let made = match answer {
        Ok((StatusCode::CREATED, made)) => made,
        Ok((_, refused)) => {
            return Outcome::Failed(format!(
                "{} could not be made on {name}: {}",
                want.name,
                said(&refused)
            ));
        }
        Err(lost) => {
            return Outcome::Failed(format!(
                "{} may have been made on {name}, but no answer came: {lost}",
                want.name
            ));
        }
    };
    let Some(workspace) = made["id"]
        .as_str()
        .and_then(|id| id.parse().ok())
        .map(WorkspaceId)
    else {
        return Outcome::Failed(format!(
            "{} was made on {name}, but its id could not be read",
            want.name
        ));
    };
    let why =
        match api::start_on(api, machine, workspace, &want.model, want.prompt.as_deref()).await {
            Ok((StatusCode::CREATED, _)) => return Outcome::Placed,
            Ok((_, refused)) => said(&refused),
            Err(refused) => refused.to_string(),
        };
    Outcome::Failed(format!(
        "{} was made on {name}, but its session did not start: {why}",
        want.name
    ))
}

fn said(answer: &serde_json::Value) -> String {
    answer["error"]
        .as_str()
        .unwrap_or("no reason was given")
        .to_owned()
}

impl Source for Queue {
    fn items(&self, fleet: &Fleet) -> Vec<Item> {
        lock(&self.requests)
            .iter()
            .filter(|r| r.started.elapsed() >= WAITING_ALERT_AFTER)
            .map(|r| {
                let name = &r.want.name;
                let (what, todo) = match &r.error {
                    Some(error) => (
                        format!("{name} was not placed: {error}"),
                        "See to it, then dismiss the request from the waiting list",
                    ),
                    None => {
                        let minutes = r.started.elapsed().as_secs() / 60;
                        let why = if r.reasons.is_empty() {
                            "no machine has sent a sample".to_owned()
                        } else {
                            r.reasons.join("; ")
                        };
                        (
                            format!("{name} has waited {minutes} minutes for a machine: {why}"),
                            "Free a machine or raise its limits, or cancel the request",
                        )
                    }
                };
                fleet.item(Kind::PlacementWaiting, r.id, self.me, what, todo.to_owned())
            })
            .collect()
    }
}

/// Tries the waiting requests every [`RETRY_EVERY`] for as long as the daemon runs.
pub async fn run(api: Api) {
    let queue = Arc::clone(&api.placements);
    let mut every = tokio::time::interval(RETRY_EVERY);
    every.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        if queue.waiting().is_empty() {
            continue;
        }
        if let Err(error) = queue.retry(&api).await {
            tracing::warn!("placement: the waiting requests were not tried: {error}");
        }
    }
}

/// `GET /api/placements`.
pub(crate) async fn list(State(api): State<Api>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "requests": api.placements.list() }))
}

/// `POST /api/placements/{id}/cancel`.
pub(crate) async fn cancel(State(api): State<Api>, Path(id): Path<String>) -> Response {
    let refused = match id.parse() {
        Ok(id) => match api.placements.cancel(id) {
            Ok(()) => return StatusCode::NO_CONTENT.into_response(),
            Err(refused) => refused,
        },
        Err(_) => Refused::Unknown,
    };
    let (status, message) = match refused {
        Refused::Unknown => (
            StatusCode::NOT_FOUND,
            format!("this machine holds no request {id:?}"),
        ),
        Refused::Placing => (
            StatusCode::CONFLICT,
            format!("request {id} is being placed now, so it can no longer be cancelled"),
        ),
    };
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::time::Duration;

    use foundry_proto::ids::{MachineId, ProjectId};
    use foundry_proto::record::{MachineSample, Record};
    use foundry_proto::settings::{Limits, SharedSettings};

    use super::*;
    use crate::attention::{Fleet, Kind};

    fn sample(owner: MachineId, sessions: u32, load_per_core: f64) -> Record<MachineSample> {
        Record {
            owner,
            version: 3,
            deleted: false,
            body: MachineSample {
                load_per_core,
                free_memory: 8_000_000_000,
                free_disk: 100_000_000_000,
                sessions,
                update_refused: None,
            },
        }
    }

    fn limits() -> Limits {
        Limits {
            max_sessions: 4,
            max_load_per_core: 1.0,
            min_free_disk: 10_000_000_000,
        }
    }

    fn limited(machines: &[MachineId]) -> BTreeMap<MachineId, Limits> {
        machines.iter().map(|m| (*m, limits())).collect()
    }

    fn all(machines: &[MachineId]) -> BTreeSet<MachineId> {
        machines.iter().copied().collect()
    }

    fn texts(reasons: &[Reason], names: &BTreeMap<MachineId, &str>) -> Vec<String> {
        reasons.iter().map(|r| r.text(names[&r.machine])).collect()
    }

    #[test]
    fn the_lowest_load_per_core_wins_among_machines_under_their_limits() {
        let [a, b, c] = [
            MachineId::random(),
            MachineId::random(),
            MachineId::random(),
        ];
        let samples = [sample(a, 1, 0.6), sample(b, 3, 0.2), sample(c, 0, 1.4)];
        // c has the fewest sessions but is over its load limit.
        assert_eq!(
            place(&samples, &limited(&[a, b, c]), &all(&[a, b, c])),
            Ok(b)
        );
    }

    #[test]
    fn fewer_sessions_break_a_tie_on_load() {
        let [a, b] = [MachineId::random(), MachineId::random()];
        for samples in [
            [sample(a, 3, 0.5), sample(b, 1, 0.5)],
            [sample(b, 1, 0.5), sample(a, 3, 0.5)],
        ] {
            assert_eq!(place(&samples, &limited(&[a, b]), &all(&[a, b])), Ok(b));
        }
    }

    #[test]
    fn an_offline_machine_is_never_picked() {
        let [a, b] = [MachineId::random(), MachineId::random()];
        let samples = [sample(a, 0, 0.0), sample(b, 2, 0.9)];
        assert_eq!(place(&samples, &limited(&[a, b]), &all(&[b])), Ok(b));
        let names = BTreeMap::from([(a, "old-mini"), (b, "vps")]);
        let refused = place(&samples[..1], &limited(&[a, b]), &all(&[b])).unwrap_err();
        assert_eq!(texts(&refused, &names), ["old-mini: offline"]);
    }

    #[test]
    fn a_machine_with_no_limits_has_none_to_be_over() {
        let [a, b] = [MachineId::random(), MachineId::random()];
        // a has no limits in the settings, so even 40 sessions at load 9 are not over any.
        let samples = [sample(a, 40, 9.0), sample(b, 5, 2.0)];
        assert_eq!(place(&samples, &limited(&[b]), &all(&[a, b])), Ok(a));
    }

    #[test]
    fn with_none_qualifying_each_machine_gives_one_reason_naming_every_limit_it_is_over() {
        let [mac, vps, mini] = [
            MachineId::random(),
            MachineId::random(),
            MachineId::random(),
        ];
        let names = BTreeMap::from([(mac, "trilogy-mac"), (vps, "vps"), (mini, "old-mini")]);
        let samples = [
            sample(mac, 1, 1.8),
            sample(vps, 4, 1.5),
            sample(mini, 0, 0.1),
        ];
        let refused = place(&samples, &limited(&[mac, vps, mini]), &all(&[mac, vps])).unwrap_err();
        assert_eq!(
            texts(&refused, &names),
            [
                "trilogy-mac: load 1.8 over limit 1.0",
                "vps: sessions 4 at limit 4, load 1.5 over limit 1.0",
                "old-mini: offline",
            ]
        );
    }

    #[test]
    fn no_machine_at_all_gives_no_machine_and_no_reason() {
        assert_eq!(
            place(&[], &BTreeMap::new(), &BTreeSet::new()),
            Err(Vec::new())
        );
    }

    fn want(name: &str) -> Want {
        Want {
            project: ProjectId::random(),
            base: "main".into(),
            name: name.into(),
            model: "opus".into(),
            prompt: None,
        }
    }

    fn fleet(me: MachineId) -> Fleet {
        Fleet {
            me,
            records: Vec::new(),
            settings: SharedSettings::default(),
            online: BTreeSet::from([me]),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_waiting_over_5_minutes_raises_placement_waiting() {
        let me = MachineId::random();
        let queue = Queue::new(me);
        let id = queue.wait(
            want("fix login"),
            vec!["studio: load 1.8 over limit 1.0".into()],
        );
        assert!(queue.items(&fleet(me)).is_empty());
        tokio::time::advance(Duration::from_secs(4 * 60 + 59)).await;
        assert!(
            queue.items(&fleet(me)).is_empty(),
            "raised before 5 minutes"
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        let items = queue.items(&fleet(me));
        assert_eq!(items.len(), 1, "{items:?}");
        let item = &items[0];
        assert_eq!(item.kind, Kind::PlacementWaiting);
        assert_eq!(item.id, format!("placement_waiting:{id}"));
        assert_eq!(item.machine, me);
        assert!(item.what.contains("fix login"), "{}", item.what);
        assert!(
            item.what.contains("studio: load 1.8 over limit 1.0"),
            "{}",
            item.what
        );
    }

    #[tokio::test(start_paused = true)]
    async fn placing_or_cancelling_a_request_clears_placement_waiting() {
        let me = MachineId::random();
        let queue = Queue::new(me);
        let placed = queue.wait(want("placed"), Vec::new());
        let cancelled = queue.wait(want("cancelled"), Vec::new());
        tokio::time::advance(WAITING_ALERT_AFTER).await;
        assert_eq!(queue.items(&fleet(me)).len(), 2);

        assert!(queue.begin(placed).is_some());
        queue.finish(placed, Outcome::Placed);
        queue.cancel(cancelled).unwrap();
        assert!(queue.items(&fleet(me)).is_empty());
        assert!(queue.list().is_empty());
    }

    #[test]
    fn a_cancelled_request_cannot_begin_and_one_being_placed_cannot_be_cancelled() {
        let queue = Queue::new(MachineId::random());
        let cancelled = queue.wait(want("a"), Vec::new());
        queue.cancel(cancelled).unwrap();
        assert!(queue.begin(cancelled).is_none());
        assert_eq!(queue.cancel(cancelled), Err(Refused::Unknown));

        let placing = queue.wait(want("b"), Vec::new());
        assert!(queue.begin(placing).is_some());
        assert_eq!(queue.cancel(placing), Err(Refused::Placing));
        // A placement that found no machine to take it waits again, and can be cancelled.
        queue.finish(placing, Outcome::Wait(None));
        queue.cancel(placing).unwrap();
    }

    #[test]
    fn a_failed_placement_is_listed_with_its_error_until_cancelled() {
        let queue = Queue::new(MachineId::random());
        let id = queue.wait(want("a"), Vec::new());
        assert!(queue.begin(id).is_some());
        queue.finish(id, Outcome::Failed("the branch a already exists".into()));
        let listed = queue.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].error.as_deref(),
            Some("the branch a already exists")
        );
        assert!(queue.begin(id).is_none(), "a failed request is tried again");
        queue.cancel(id).unwrap();
        assert!(queue.list().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_request_keeps_its_placement_waiting_item_until_it_is_dismissed() {
        let me = MachineId::random();
        let queue = Queue::new(me);
        let id = queue.wait(want("docs"), Vec::new());
        assert!(queue.begin(id).is_some());
        queue.finish(
            id,
            Outcome::Failed("docs could not be made on vps: taken".into()),
        );
        tokio::time::advance(WAITING_ALERT_AFTER).await;
        let items = queue.items(&fleet(me));
        assert_eq!(items.len(), 1, "{items:?}");
        assert!(
            items[0]
                .what
                .contains("docs could not be made on vps: taken"),
            "{}",
            items[0].what
        );
        queue.cancel(id).unwrap();
        assert!(queue.items(&fleet(me)).is_empty());
    }

    #[test]
    fn stopping_drops_every_request() {
        let queue = Queue::new(MachineId::random());
        queue.wait(want("a"), Vec::new());
        queue.wait(want("b"), Vec::new());
        assert_eq!(queue.drop_all(), 2);
        assert!(queue.list().is_empty());
    }
}
