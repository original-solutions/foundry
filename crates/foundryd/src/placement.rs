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
//! and first prompt. Machines are picked for the waiting requests one at a time, in the order
//! they came; each create and session start then runs as a task of its own, so a machine slow
//! to answer one request holds back no other.
//!
//! - `GET /api/placements`: `{"requests": [...]}`, each a [`Listed`].
//! - `POST /api/placements/{id}/cancel`: `204`, and the request is never placed. One being
//!   placed now is `409`; an id this machine holds no request for is `404`.
//!
//! A create sent to a machine counts as one more session on it, in every pick, until the
//! second sample newer than the version held once the create was kept (the first may have been
//! written before its session started), so its limits hold before its samples catch up. A
//! create that failed, or whose request went away mid-create, counts for nothing. A machine
//! that cannot take the call (offline, older, its link down) is passed over until its next
//! sample, with that reason, and the next roomiest is tried; a call that may have run there is
//! never sent anywhere else.
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
use tokio::task::JoinSet;
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

/// A machine passed over until a sample newer than `version` arrives, as it refused a call
/// before it was sent, for the reason `why`.
struct Bench {
    version: u64,
    why: String,
}

/// A create sent to `machine` whose session its samples do not count yet. Each pick counts it
/// as one more session on that machine, until the second sample newer than the version held
/// once it was kept: the first may have been written before its session started.
struct Pending {
    id: Uuid,
    machine: MachineId,
    /// The newest version of `machine` known not to count the session; `None` while the create
    /// is in flight.
    counted_after: Option<u64>,
    /// Whether one newer sample has been seen since it was kept.
    rearmed: bool,
}

type Pendings = Arc<Mutex<Vec<Pending>>>;

/// A create's claim on a session of the machine picked for it. Dropped without being kept (its
/// create failed, or the request that made it went away mid-create), it counts no more.
pub(crate) struct Reservation {
    pendings: Pendings,
    id: Uuid,
    kept: bool,
}

impl Reservation {
    /// Keeps counting the session until the second sample of its machine newer than `held`,
    /// the version held once it was made.
    pub(crate) fn keep_until(mut self, held: u64) {
        if let Some(p) = lock(&self.pendings).iter_mut().find(|p| p.id == self.id) {
            p.counted_after = Some(held);
        }
        self.kept = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.kept {
            lock(&self.pendings).retain(|p| p.id != self.id);
        }
    }
}

/// What a pick reads: each machine's newest sample, the settings and who is online.
struct View {
    samples: Vec<Record<MachineSample>>,
    settings: SharedSettings,
    online: BTreeSet<MachineId>,
}

impl View {
    fn name(&self, machine: MachineId) -> String {
        self.settings
            .machines
            .iter()
            .find(|m| m.id == machine)
            .map_or_else(|| machine.to_string(), |m| m.name.clone())
    }

    fn newest(&self, machine: MachineId) -> Option<&Record<MachineSample>> {
        self.samples.iter().find(|s| s.owner == machine)
    }
}

/// A machine picked for a create, before anything is sent to it.
pub(crate) struct Picked {
    machine: MachineId,
    /// The machine's name, for messages.
    name: String,
    /// The version of its newest sample at the pick, which a bench of it lasts past.
    version: u64,
    /// The session the create brings, counted against the machine from the pick on.
    pending: Reservation,
}

/// A create sent to the machine picked for it, and what that machine answered.
pub(crate) struct Sent {
    pub machine: MachineId,
    /// The machine's name, for messages.
    pub name: String,
    pub answer: Result<(StatusCode, serde_json::Value), CallError>,
    /// The session it brings, counted against the machine until
    /// [`settle`](Queue::settle) says whether it was made.
    pub pending: Reservation,
}

/// What a reason says of a machine that refused a call before it was sent, or `None` when the
/// call may have run there. An answer that could not be read is a `Refused` after the call ran,
/// so `Refused` may have run.
fn unreachable(refused: &CallError) -> Option<String> {
    match refused {
        CallError::Lost(_) | CallError::Refused(_) => None,
        CallError::NotListed(_) => Some("not in the machine list".to_owned()),
        CallError::Offline(_) => Some("offline".to_owned()),
        CallError::Older(_) => Some("runs an older Foundry, which cannot take a create".to_owned()),
        CallError::Down(_) => Some("its link is down".to_owned()),
    }
}

/// This machine's requests waiting for a machine, in the order they came.
pub struct Queue {
    me: MachineId,
    requests: Mutex<Vec<Request>>,
    samples: Arc<Mutex<Samples>>,
    benched: Mutex<BTreeMap<MachineId, Bench>>,
    pending: Pendings,
}

impl Queue {
    pub fn new(me: MachineId) -> Self {
        Self {
            me,
            requests: Mutex::default(),
            samples: Arc::default(),
            benched: Mutex::default(),
            pending: Arc::default(),
        }
    }

    /// Sends a create of `name` from `base` in `project` to the roomiest machine that takes the
    /// call, or gives the reasons no machine qualifies, as the UI shows them. A machine that
    /// refuses the call before it is sent is benched and the next is tried. The create sent
    /// counts as a session on its machine until the caller settles it.
    pub(crate) async fn create(
        &self,
        api: &Api,
        project: ProjectId,
        base: &str,
        name: &str,
    ) -> Result<Result<Sent, Vec<String>>, StoreError> {
        match self.choose(api, &BTreeSet::new()).await? {
            Ok(picked) => self.send(api, picked, project, base, name).await,
            Err(reasons) => Ok(Err(reasons)),
        }
    }

    /// The roomiest machine not in `tried`, with the create about to be sent there counted as
    /// a session on it, or the reasons no machine qualifies, as the UI shows them.
    async fn choose(
        &self,
        api: &Api,
        tried: &BTreeSet<MachineId>,
    ) -> Result<Result<Picked, Vec<String>>, StoreError> {
        let view = self.view(api).await?;
        Ok(self.pick(&view, tried).map(|(machine, pending)| Picked {
            machine,
            name: view.name(machine),
            version: view.newest(machine).map_or(0, |s| s.version),
            pending,
        }))
    }

    /// Sends a create of `name` from `base` in `project` to the machine `picked`. When that
    /// machine refuses the call before it is sent, it is benched and the next roomiest is
    /// picked and tried, until one takes the call or none qualifies.
    async fn send(
        &self,
        api: &Api,
        mut picked: Picked,
        project: ProjectId,
        base: &str,
        name: &str,
    ) -> Result<Result<Sent, Vec<String>>, StoreError> {
        // Each machine is tried once per create, even when a new sample takes it off the bench.
        let mut tried = BTreeSet::new();
        loop {
            let Picked {
                machine,
                name: called,
                version,
                pending,
            } = picked;
            tried.insert(machine);
            let answer = api::create_on(api, machine, project, base, name).await;
            let Some(why) = answer.as_ref().err().and_then(unreachable) else {
                return Ok(Ok(Sent {
                    machine,
                    name: called,
                    answer,
                    pending,
                }));
            };
            drop(pending);
            lock(&self.benched).insert(machine, Bench { version, why });
            picked = match self.choose(api, &tried).await? {
                Ok(picked) => picked,
                Err(reasons) => return Ok(Err(reasons)),
            };
        }
    }

    /// Picks the roomiest machine not in `tried`, counting each create still pending on it as
    /// a session, and records the create about to be sent there as pending, under one lock so
    /// two picks at once never both take a machine's last session. Or gives the reasons none
    /// qualifies, each benched machine's last.
    fn pick(
        &self,
        view: &View,
        tried: &BTreeSet<MachineId>,
    ) -> Result<(MachineId, Reservation), Vec<String>> {
        let mut benched = lock(&self.benched);
        benched.retain(|machine, bench| {
            view.newest(*machine)
                .is_some_and(|s| s.version <= bench.version)
        });
        let mut pending = lock(&self.pending);
        age(&mut pending, view);
        let candidates: Vec<Record<MachineSample>> = view
            .samples
            .iter()
            .filter(|s| !benched.contains_key(&s.owner) && !tried.contains(&s.owner))
            .map(|s| {
                let brought = pending.iter().filter(|p| p.machine == s.owner).count();
                let mut s = s.clone();
                s.body.sessions = s
                    .body
                    .sessions
                    .saturating_add(u32::try_from(brought).unwrap_or(u32::MAX));
                s
            })
            .collect();
        match place(&candidates, &view.settings.limits, &view.online) {
            Ok(machine) => {
                let id = Uuid::new_v4();
                pending.push(Pending {
                    id,
                    machine,
                    counted_after: None,
                    rearmed: false,
                });
                let reservation = Reservation {
                    pendings: Arc::clone(&self.pending),
                    id,
                    kept: false,
                };
                Ok((machine, reservation))
            }
            Err(reasons) => {
                let benched = benched
                    .iter()
                    .map(|(machine, bench)| format!("{}: {}", view.name(*machine), bench.why));
                Err(reasons
                    .iter()
                    .map(|reason| reason.text(&view.name(reason.machine)))
                    .chain(benched)
                    .collect())
            }
        }
    }

    /// Whether any create still counts as a session on its machine.
    fn counting(&self) -> bool {
        !lock(&self.pending).is_empty()
    }

    /// Reads the samples and lets each kept create see the newest of its machine's, so every
    /// sample is seen while creates count, not only those a pick happens to read.
    async fn watch(&self, api: &Api) -> Result<(), StoreError> {
        let view = self.view(api).await?;
        age(&mut lock(&self.pending), &view);
        Ok(())
    }

    /// Ends the create `pending` reserved on `machine`: when it `made` a session, or one is
    /// about to be started in it, the session counts until samples count it; otherwise it
    /// counts no more.
    pub(crate) async fn settle(
        &self,
        api: &Api,
        machine: MachineId,
        pending: Reservation,
        made: bool,
    ) {
        if !made {
            return;
        }
        let held = on_store(&api.store, |store| store.versions())
            .await
            .ok()
            .and_then(|versions| versions.get(&machine).copied())
            .unwrap_or(0);
        pending.keep_until(held);
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
    /// not listed; only the records written since the last read are loaded.
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
        let samples = order.iter().filter_map(|id| newest.remove(id)).collect();
        Ok(View {
            samples,
            settings,
            online,
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

    /// Tries each waiting request once, in the order they came: picks its machine here, so the
    /// picks keep that order, then sends its create and starts its session as a task of
    /// `placing`, so a machine slow to answer one request holds back no other.
    async fn retry(
        self: &Arc<Self>,
        api: &Api,
        placing: &mut JoinSet<()>,
    ) -> Result<(), StoreError> {
        for id in self.waiting() {
            let picked = match self.choose(api, &BTreeSet::new()).await? {
                Ok(picked) => picked,
                Err(reasons) => {
                    self.finish(id, Outcome::Wait(Some(reasons)));
                    continue;
                }
            };
            // Marked as being placed only once a machine is picked, so a cancel while no machine
            // qualifies is never refused. One cancelled since is not placed, and the pick,
            // dropped here, counts for nothing.
            let Some(want) = self.begin(id) else {
                continue;
            };
            let (queue, api) = (Arc::clone(self), api.clone());
            placing.spawn(async move { queue.place_picked(&api, id, want, picked).await });
        }
        Ok(())
    }

    /// Places request `id`, already marked as being placed, on the machine `picked` for it:
    /// the create, then its session.
    async fn place_picked(&self, api: &Api, id: Uuid, want: Want, picked: Picked) {
        let sent = self
            .send(api, picked, want.project, &want.base, &want.name)
            .await;
        let outcome = match sent {
            Err(error) => {
                tracing::warn!("placement: {} was not tried: {error}", want.name);
                Outcome::Wait(None)
            }
            Ok(Err(reasons)) => Outcome::Wait(Some(reasons)),
            Ok(Ok(sent)) => {
                let Sent {
                    machine,
                    name,
                    answer,
                    pending,
                } = sent;
                let outcome = start(api, &want, machine, &name, answer).await;
                let placed = matches!(outcome, Outcome::Placed);
                self.settle(api, machine, pending, placed).await;
                if placed {
                    tracing::info!("placement: {} is placed", want.name);
                }
                outcome
            }
        };
        self.finish(id, outcome);
    }
}

/// Stops counting each kept create whose machine has sent a second sample since it was kept;
/// on the first, notes it.
fn age(pending: &mut Vec<Pending>, view: &View) {
    pending.retain_mut(|p| {
        let Some(after) = p.counted_after else {
            return true;
        };
        match view.newest(p.machine) {
            Some(newest) if newest.version > after => {
                if p.rearmed {
                    return false;
                }
                p.counted_after = Some(newest.version);
                p.rearmed = true;
                true
            }
            _ => true,
        }
    });
}

/// Starts `want`'s session in the workspace the create sent to `machine`, named `name`, made,
/// as `answer` says, or says why it was not placed.
async fn start(
    api: &Api,
    want: &Want,
    machine: MachineId,
    name: &str,
    answer: Result<(StatusCode, serde_json::Value), CallError>,
) -> Outcome {
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

/// Tries the waiting requests every [`RETRY_EVERY`] for as long as the daemon runs, and while
/// none waits but creates still count, reads the samples that age them. The placements under
/// way are its tasks: they end with it, and one that panics takes it down as well.
pub async fn run(api: Api) {
    let queue = Arc::clone(&api.placements);
    let mut placing = JoinSet::new();
    let mut every = tokio::time::interval(RETRY_EVERY);
    every.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        while let Some(ended) = placing.try_join_next() {
            if let Err(error) = ended
                && error.is_panic()
            {
                std::panic::resume_unwind(error.into_panic());
            }
        }
        let tried = if !queue.waiting().is_empty() {
            queue.retry(&api, &mut placing).await
        } else if queue.counting() {
            queue.watch(&api).await
        } else {
            continue;
        };
        if let Err(error) = tried {
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
                version: None,
                update_ready: None,
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

    /// A view of `samples`, each machine limited to one session.
    fn one_session_each(samples: Vec<Record<MachineSample>>) -> View {
        let owners: Vec<MachineId> = samples.iter().map(|s| s.owner).collect();
        let mut settings = SharedSettings::default();
        for owner in &owners {
            settings.limits.insert(
                *owner,
                Limits {
                    max_sessions: 1,
                    ..limits()
                },
            );
        }
        View {
            samples,
            settings,
            online: all(&owners),
        }
    }

    fn sampled(owner: MachineId, version: u64) -> Record<MachineSample> {
        Record {
            version,
            ..sample(owner, 0, 0.1)
        }
    }

    #[test]
    fn a_create_counts_as_a_session_on_its_machine_until_it_is_dropped_unsettled() {
        let m = MachineId::random();
        let queue = Queue::new(m);
        let view = one_session_each(vec![sampled(m, 3)]);
        let (picked, reservation) = queue.pick(&view, &BTreeSet::new()).unwrap();
        assert_eq!(picked, m);
        assert!(
            queue.pick(&view, &BTreeSet::new()).is_err(),
            "its last session taken twice"
        );
        // The request went away mid-create: nothing settles it.
        drop(reservation);
        assert!(queue.pick(&view, &BTreeSet::new()).is_ok());
    }

    #[test]
    fn a_kept_create_counts_until_the_second_sample_newer_than_it() {
        let m = MachineId::random();
        let queue = Queue::new(m);
        let (_, reservation) = queue
            .pick(&one_session_each(vec![sampled(m, 3)]), &BTreeSet::new())
            .unwrap();
        reservation.keep_until(5);
        for version in [5, 6] {
            // Not newer, then newer but maybe written before the session started.
            let view = one_session_each(vec![sampled(m, version)]);
            assert!(queue.pick(&view, &BTreeSet::new()).is_err(), "at {version}");
        }
        let view = one_session_each(vec![sampled(m, 7)]);
        assert!(queue.pick(&view, &BTreeSet::new()).is_ok());
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
