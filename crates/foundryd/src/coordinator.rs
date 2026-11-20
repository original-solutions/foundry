//! The priority order and the shared settings sequence.
//!
//! [`acting`] says which machine holds the coordinator role: the first machine in the priority
//! order that this machine can reach. Only that machine numbers edits to the shared settings
//! ([`numbered`]); every other machine forwards an edit to it. Copies of the settings meet
//! through [`merge`], one setting at a time, so a machine never loses an edit it has not
//! passed on yet, and every machine reaches the same result whatever order copies arrive in.
//!
//! "Later" is the higher number, never wall time: of two edits to one setting the higher
//! stamp holds ([`Stamp`]'s order). Two stamps conflict only when neither copy has seen the
//! other's, which is two sides of a split each editing the setting. A conflict leaves a
//! "setting overwritten" item, itself a setting, holding both values.
//!
//! [`Coordinator`] runs this on a machine. Record links hand it every copy of the settings a
//! peer sends and every edit forwarded to this machine ([`crate::sync::Peers::coordinate`]).
//! A machine that comes to act numbers nothing until it has taken a copy from every other
//! machine it can reach, so a returning machine catches up before it edits; an edit that
//! reaches it before then waits, at most [`OFFLINE_AFTER`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use foundry_proto::ids::MachineId;
use foundry_proto::messages::{PROTOCOL, PeerMessage};
use foundry_proto::settings::{
    Edit, Overwritten, SettingKey, SettingValue, SharedSettings, Stamp, StampKey,
};
use serde::Serialize;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::store::{Store, StoreError};
use crate::sync::{OFFLINE_AFTER, Peers, on_store};

/// How often an edit that waits looks again.
const STEP: Duration = Duration::from_millis(100);

/// How long an edit a peer forwarded is kept, from when it arrived, while this machine has no
/// open link to the machine it takes to act. A machine silent for [`OFFLINE_AFTER`] is no
/// longer reachable, so by twice that the role has moved to one this machine can reach.
const KEEP_FORWARDED: Duration = OFFLINE_AFTER.saturating_mul(2);

/// Why an edit was not made.
#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("no link to {0}, the acting coordinator, is open, so the edit was not sent")]
    NotLinked(MachineId),
    #[error(
        "an edit {from} forwarded was dropped: {KEEP_FORWARDED:?} after it arrived this \
         machine still has no open link to {acting}, which acts as it sees it"
    )]
    Unsettled { from: MachineId, acting: MachineId },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// This machine's part in the coordinator role: it numbers edits while it acts, forwards them
/// while another machine does, and merges every copy of the settings a peer sends.
pub struct Coordinator {
    me: MachineId,
    store: Arc<Mutex<Store>>,
    peers: Peers,
    /// The machines this one has taken a copy of the settings from, and when it last took one.
    /// Each link sends its copy before anything else, so a machine is in here by the time it
    /// is heard from.
    taken: Mutex<BTreeMap<MachineId, Instant>>,
    /// When this machine started. Until every listed machine's copy is in, or
    /// [`OFFLINE_AFTER`] has passed, its own copy may be an old one.
    started: Instant,
    /// The task making the edit forwarded last. Each forwarded edit waits for the one before,
    /// so they are numbered in the order they arrived.
    last_forwarded: Mutex<Option<JoinHandle<()>>>,
}

impl Coordinator {
    pub fn new(me: MachineId, store: Arc<Mutex<Store>>, peers: Peers) -> Self {
        Self {
            me,
            store,
            peers,
            taken: Mutex::new(BTreeMap::new()),
            started: Instant::now(),
            last_forwarded: Mutex::new(None),
        }
    }

    /// The machine that holds the role, as this machine sees it now.
    pub async fn acting(&self) -> Result<MachineId, StoreError> {
        let held = self.held().await?;
        Ok(acting(&held.priority, &self.peers.online()))
    }

    /// Makes `edit`. On the acting machine it is numbered once this machine has caught up, or
    /// after [`OFFLINE_AFTER`]. On any other machine it is sent to the acting one, and this
    /// returns without waiting for it to be numbered: the change shows when the settings
    /// arrive. It fails only when no link to the acting machine is open, or the store fails.
    pub async fn edit(&self, edit: Edit) -> Result<(), CoordinatorError> {
        self.edit_from(None, Instant::now(), edit).await
    }

    /// Merges a copy of the settings `peer` sent into the held copy.
    pub(crate) async fn take_settings(
        &self,
        peer: MachineId,
        settings: SharedSettings,
    ) -> Result<(), StoreError> {
        on_store(&self.store, move |store| {
            let held = store.settings()?;
            let merged = merge(&held, &settings);
            if merged != held {
                store.apply_settings(merged)?;
            }
            Ok::<_, StoreError>(())
        })
        .await?;
        lock(&self.taken).insert(peer, Instant::now());
        Ok(())
    }

    /// Makes an edit `peer` forwarded, in the background, so the link that carried it goes on
    /// reading while the edit waits. Forwarded edits are made one at a time, in the order they
    /// arrived, and each one's waits count from when it arrived, not from its turn.
    pub(crate) fn take_edit(self: Arc<Self>, peer: MachineId, edit: Edit) {
        let arrived = Instant::now();
        let coordinator = Arc::clone(&self);
        let mut last = lock(&self.last_forwarded);
        let before = last.take();
        *last = Some(tokio::spawn(async move {
            if let Some(before) = before {
                let _ = before.await;
            }
            if let Err(error) = coordinator.edit_from(Some(peer), arrived, edit).await {
                tracing::warn!("coordinator: {error}");
            }
        }));
    }

    /// Makes `edit`, which `from` forwarded, or this machine's own when `None`; `arrived` is
    /// when it reached this machine. Only the acting machine numbers it, once caught up or
    /// [`OFFLINE_AFTER`] after it arrived.
    ///
    /// A forwarded edit looks again every [`STEP`] while it cannot go on. While this machine
    /// takes the forwarding machine to act, the two may still disagree on who acts, so the edit
    /// goes back only once their views agree: when a copy of the settings taken from that
    /// machine after the edit arrived still leaves it acting, or at the latest
    /// [`OFFLINE_AFTER`] after the edit arrived. The forwarding machine, acting, then numbers
    /// it. While there is no open link to the acting machine it is kept until
    /// [`KEEP_FORWARDED`].
    async fn edit_from(
        &self,
        from: Option<MachineId>,
        arrived: Instant,
        edit: Edit,
    ) -> Result<(), CoordinatorError> {
        loop {
            let held = self.held().await?;
            let acting = acting(&held.priority, &self.peers.online());
            let waited = arrived.elapsed();
            if acting == self.me {
                if self.caught_up(&held) || waited >= OFFLINE_AFTER {
                    return self.number(edit).await;
                }
            } else if Some(acting) != from
                || waited >= OFFLINE_AFTER
                || self.heard_since(acting, arrived)
            {
                let message = PeerMessage::Edit {
                    protocol: PROTOCOL,
                    edit: edit.clone(),
                };
                if self.peers.send(acting, &message).await {
                    return Ok(());
                }
                if from.is_none() {
                    return Err(CoordinatorError::NotLinked(acting));
                }
            }
            if let Some(from) = from
                && waited >= KEEP_FORWARDED
            {
                return Err(CoordinatorError::Unsettled { from, acting });
            }
            tokio::time::sleep(STEP).await;
        }
    }

    /// Gives `edit` the next number, on the copy held at that moment.
    async fn number(&self, edit: Edit) -> Result<(), CoordinatorError> {
        let me = self.me;
        on_store(&self.store, move |store| {
            let held = store.settings()?;
            store.apply_settings(numbered(&held, &edit, me))?;
            Ok(())
        })
        .await
    }

    /// Whether a copy of the settings was taken from `peer` after `at`.
    fn heard_since(&self, peer: MachineId, at: Instant) -> bool {
        lock(&self.taken)
            .get(&peer)
            .is_some_and(|taken| *taken > at)
    }

    async fn held(&self) -> Result<SharedSettings, StoreError> {
        on_store(&self.store, |store| store.settings()).await
    }

    /// Whether this machine holds what it needs to number: a copy from every other machine it
    /// can reach, and, in its first [`OFFLINE_AFTER`], from every machine `held` lists, so a
    /// machine that has just started and hears from no one yet does not number on an old copy.
    fn caught_up(&self, held: &SharedSettings) -> bool {
        let taken = lock(&self.taken);
        let has = |id: &MachineId| *id == self.me || taken.contains_key(id);
        let reachable = self.peers.online().iter().all(has);
        let listed = held.machines.iter().all(|m| has(&m.id));
        reachable && (listed || self.started.elapsed() >= OFFLINE_AFTER)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The machine that holds the coordinator role: the first in `priority` that is in
/// `reachable`, else the lowest id in `reachable`. `reachable` holds the asking machine, so is
/// never empty in use; when it is, the first in `priority` acts, or the nil id.
pub fn acting(priority: &[MachineId], reachable: &BTreeSet<MachineId>) -> MachineId {
    priority
        .iter()
        .find(|id| reachable.contains(id))
        .or_else(|| reachable.first())
        .or_else(|| priority.first())
        .copied()
        .unwrap_or(Stamp::NONE.by)
}

/// `held` with `edit` applied as the next number, given by `me`: the setting takes the stamp
/// `{held.seq + 1, me}`.
pub fn numbered(held: &SharedSettings, edit: &Edit, me: MachineId) -> SharedSettings {
    let seq = held.seq.saturating_add(1);
    let mut next = held.clone();
    let value = edit.value().map(Value::Setting);
    put(&mut next, &edit.key(), value, Stamp { seq, by: me });
    next.seen.insert(me, seq);
    next.seq = seq;
    next
}

/// One copy of the settings from two, setting by setting: the higher stamp's value holds. A
/// setting both copies edited where neither had seen the other's edit also gets a "setting
/// overwritten" item holding both values, stamped as the edit that held. `seen` is the higher
/// of each.
///
/// The result's `seq` is the higher of the two, plus one unless the result is the settings of
/// every copy that holds that `seq`. That is one more case than "differs from both": a result
/// that is only the lower copy's settings also takes a new number. A store takes a copy only
/// with a higher `seq` than it holds, so a store holding the top copy would refuse such a
/// result if it kept the top `seq`, and the lower copy's edits would never reach it.
pub fn merge(a: &SharedSettings, b: &SharedSettings) -> SharedSettings {
    let mut out = SharedSettings::default();
    let mut conflicts = Vec::new();
    for key in keys(a).union(&keys(b)) {
        let (sa, sb) = (stamp(a, key), stamp(b, key));
        let (va, vb) = (get(a, key), get(b, key));
        let a_holds = match sa.cmp(&sb) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            // One edit, or two copies no edit has stamped: pick by the value, the same way on
            // every machine.
            std::cmp::Ordering::Equal => text(&va) >= text(&vb),
        };
        let (held, lost, stamp) = if a_holds { (va, vb, sa) } else { (vb, va, sb) };
        // Two sides that set one value raise nothing: no value was lost.
        if let StampKey::Setting(setting) = key
            && held != lost
            && !covers(&a.seen, sb)
            && !covers(&b.seen, sa)
        {
            let item = Overwritten {
                kept: self::setting(&held),
                lost: self::setting(&lost),
            };
            conflicts.push((setting.clone(), item, stamp));
        }
        put(&mut out, key, held, stamp);
    }
    for (key, item, stamp) in conflicts {
        let key = StampKey::Overwritten(key);
        if stamp > self::stamp(&out, &key) {
            put(&mut out, &key, Some(Value::Item(item)), stamp);
        }
    }
    out.seen = a.seen.clone();
    for (by, seq) in &b.seen {
        let seen = out.seen.entry(*by).or_default();
        *seen = (*seen).max(*seq);
    }
    let top = a.seq.max(b.seq);
    let is_top = [a, b]
        .iter()
        .filter(|copy| copy.seq == top)
        .all(|copy| same_settings(copy, &out));
    out.seq = if is_top { top } else { top.saturating_add(1) };
    out
}

/// Whether two copies hold the same settings, whatever their `seq`.
fn same_settings(a: &SharedSettings, b: &SharedSettings) -> bool {
    SharedSettings {
        seq: 0,
        ..a.clone()
    } == SharedSettings {
        seq: 0,
        ..b.clone()
    }
}

/// Whether a copy that has seen `seen` includes the edit stamped `stamp`.
fn covers(seen: &BTreeMap<MachineId, u64>, stamp: Stamp) -> bool {
    stamp == Stamp::NONE || seen.get(&stamp.by).is_some_and(|seq| *seq >= stamp.seq)
}

/// The value of one setting: a plain setting's, or an overwritten item.
#[derive(Clone, Debug, PartialEq, Serialize)]
enum Value {
    Setting(SettingValue),
    Item(Overwritten),
}

fn setting(value: &Option<Value>) -> Option<SettingValue> {
    match value {
        Some(Value::Setting(v)) => Some(v.clone()),
        _ => None,
    }
}

/// `value` as JSON text, to order two values the same way on every machine.
fn text(value: &Option<Value>) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// Every setting `settings` holds a value or a stamp for.
fn keys(settings: &SharedSettings) -> BTreeSet<StampKey> {
    let mut keys: BTreeSet<StampKey> = settings.stamps.keys().cloned().collect();
    let mut plain: Vec<SettingKey> = Vec::new();
    plain.extend(settings.machines.iter().map(|m| SettingKey::Machine(m.id)));
    plain.extend(settings.projects.iter().map(|p| SettingKey::Project(p.id)));
    plain.extend(settings.limits.keys().map(|id| SettingKey::Limits(*id)));
    if !settings.priority.is_empty() {
        plain.push(SettingKey::Priority);
    }
    keys.extend(plain.into_iter().map(StampKey::Setting));
    keys.extend(
        settings
            .overwritten
            .keys()
            .cloned()
            .map(StampKey::Overwritten),
    );
    keys
}

fn stamp(settings: &SharedSettings, key: &StampKey) -> Stamp {
    settings.stamps.get(key).copied().unwrap_or(Stamp::NONE)
}

/// The value `settings` holds for `key`, `None` when it holds none. An empty priority order is
/// none.
fn get(settings: &SharedSettings, key: &StampKey) -> Option<Value> {
    let key = match key {
        StampKey::Overwritten(key) => {
            return settings.overwritten.get(key).cloned().map(Value::Item);
        }
        StampKey::Setting(key) => key,
    };
    let value = match key {
        SettingKey::Machine(id) => settings
            .machines
            .iter()
            .find(|m| m.id == *id)
            .cloned()
            .map(SettingValue::Machine),
        SettingKey::Priority => (!settings.priority.is_empty())
            .then(|| SettingValue::Priority(settings.priority.clone())),
        SettingKey::Project(id) => settings
            .projects
            .iter()
            .find(|p| p.id == *id)
            .cloned()
            .map(SettingValue::Project),
        SettingKey::Limits(id) => settings.limits.get(id).cloned().map(SettingValue::Limits),
    };
    value.map(Value::Setting)
}

/// Sets `key` to `value` (`None` removes it) with `stamp`. Machines and projects are kept in
/// id order, so two machines holding the same settings hold the same lists. A value of the
/// wrong kind for `key` is taken as none.
fn put(settings: &mut SharedSettings, key: &StampKey, value: Option<Value>, stamp: Stamp) {
    match key {
        StampKey::Setting(SettingKey::Machine(id)) => {
            settings.machines.retain(|m| m.id != *id);
            if let Some(Value::Setting(SettingValue::Machine(m))) = value
                && m.id == *id
            {
                settings.machines.push(m);
                settings.machines.sort_by_key(|m| m.id);
            }
        }
        StampKey::Setting(SettingKey::Priority) => {
            settings.priority = match value {
                Some(Value::Setting(SettingValue::Priority(order))) => order,
                _ => Vec::new(),
            };
        }
        StampKey::Setting(SettingKey::Project(id)) => {
            settings.projects.retain(|p| p.id != *id);
            if let Some(Value::Setting(SettingValue::Project(p))) = value
                && p.id == *id
            {
                settings.projects.push(p);
                settings.projects.sort_by_key(|p| p.id);
            }
        }
        StampKey::Setting(SettingKey::Limits(id)) => match value {
            Some(Value::Setting(SettingValue::Limits(limits))) => {
                settings.limits.insert(*id, limits);
            }
            _ => {
                settings.limits.remove(id);
            }
        },
        StampKey::Overwritten(inner) => match value {
            Some(Value::Item(item)) => {
                settings.overwritten.insert(inner.clone(), item);
            }
            _ => {
                settings.overwritten.remove(inner);
            }
        },
    }
    if stamp == Stamp::NONE {
        settings.stamps.remove(key);
    } else {
        settings.stamps.insert(key.clone(), stamp);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use foundry_proto::ids::{MachineId, ProjectId};
    use foundry_proto::settings::{
        Edit, Limits, Machine, Overwritten, Project, SettingKey, SettingValue, SharedSettings,
        Stamp, StampKey,
    };

    use super::*;

    /// Machine ids in a known order: `id(1) < id(2) < …`.
    fn id(n: u128) -> MachineId {
        MachineId(uuid::Uuid::from_u128(n))
    }

    fn reachable(ids: &[MachineId]) -> BTreeSet<MachineId> {
        ids.iter().copied().collect()
    }

    #[test]
    fn the_first_reachable_machine_in_the_order_acts() {
        let (a, b, c) = (id(3), id(1), id(2));
        let order = [a, b, c];
        assert_eq!(acting(&order, &reachable(&[a, b, c])), a);
        assert_eq!(acting(&order, &reachable(&[b, c])), b);
        assert_eq!(acting(&order, &reachable(&[c])), c);
    }

    #[test]
    fn with_no_reachable_machine_in_the_order_the_lowest_reachable_id_acts() {
        let (a, b, c) = (id(3), id(1), id(2));
        assert_eq!(acting(&[a], &reachable(&[b, c])), b);
        assert_eq!(acting(&[], &reachable(&[c, a])), c);
    }

    const A: u128 = 1;
    const B: u128 = 2;

    fn project(n: u128, name: &str) -> Project {
        Project {
            id: ProjectId(uuid::Uuid::from_u128(n)),
            name: name.into(),
            repo: format!("https://example.invalid/{name}.git"),
        }
    }

    fn limits(max_sessions: u32) -> Limits {
        Limits {
            max_sessions,
            max_load_per_core: 1.0,
            min_free_disk: 1,
        }
    }

    /// Two machines, both listed, `a` first, as both held them before any split.
    fn base() -> SharedSettings {
        let machines = [id(A), id(B)]
            .iter()
            .enumerate()
            .map(|(i, id)| Machine {
                id: *id,
                name: format!("m{i}"),
                address: format!("100.64.0.{i}"),
            })
            .collect();
        SharedSettings {
            seq: 1,
            machines,
            priority: vec![id(A), id(B)],
            ..SharedSettings::default()
        }
    }

    fn set_project(n: u128, name: &str) -> Edit {
        Edit::SetProject {
            project: project(n, name),
        }
    }

    fn project_named(settings: &SharedSettings, n: u128) -> Option<String> {
        settings
            .projects
            .iter()
            .find(|p| p.id.0 == uuid::Uuid::from_u128(n))
            .map(|p| p.name.clone())
    }

    /// Merges both ways, checks both orders agree and merging again changes nothing.
    fn merged(a: &SharedSettings, b: &SharedSettings) -> SharedSettings {
        let ab = merge(a, b);
        let ba = merge(b, a);
        assert_eq!(ab, ba, "the merge depends on its order");
        assert_eq!(
            merge(&ab, a),
            ab,
            "merging a side in again changes the result"
        );
        assert_eq!(
            merge(&ab, b),
            ab,
            "merging a side in again changes the result"
        );
        assert_eq!(merge(&ab, &ab), ab, "a merge with itself changes it");
        ab
    }

    #[test]
    fn an_edit_takes_the_next_number_and_stamps_its_setting() {
        let held = base();
        let next = numbered(&held, &set_project(7, "p"), id(B));
        assert_eq!(next.seq, 2);
        assert_eq!(next.seen, [(id(B), 2)].into());
        assert_eq!(
            next.stamps[&StampKey::Setting(SettingKey::Project(project(7, "p").id))],
            Stamp { seq: 2, by: id(B) }
        );
        assert_eq!(project_named(&next, 7).as_deref(), Some("p"));
        assert_eq!(next.machines, held.machines);
        assert_eq!(next.priority, held.priority);
    }

    #[test]
    fn a_removal_keeps_its_stamp() {
        let held = numbered(&base(), &set_project(7, "p"), id(A));
        let id7 = project(7, "p").id;
        let next = numbered(&held, &Edit::RemoveProject { id: id7 }, id(A));
        assert_eq!(project_named(&next, 7), None);
        assert_eq!(
            next.stamps[&StampKey::Setting(SettingKey::Project(id7))],
            Stamp { seq: 3, by: id(A) }
        );
    }

    #[test]
    fn each_kind_of_edit_changes_its_setting() {
        let mut held = base();
        let c = Machine {
            id: id(9),
            name: "c".into(),
            address: "100.64.0.9".into(),
        };
        held = numbered(&held, &Edit::SetMachine { machine: c.clone() }, id(A));
        assert!(held.machines.contains(&c));
        held = numbered(&held, &Edit::Priority { order: vec![id(9)] }, id(A));
        assert_eq!(held.priority, vec![id(9)]);
        held = numbered(
            &held,
            &Edit::SetLimits {
                machine: id(9),
                limits: limits(3),
            },
            id(A),
        );
        assert_eq!(held.limits[&id(9)], limits(3));
        held = numbered(&held, &Edit::RemoveLimits { machine: id(9) }, id(A));
        assert!(!held.limits.contains_key(&id(9)));
        held = numbered(&held, &Edit::RemoveMachine { id: id(9) }, id(A));
        assert!(!held.machines.contains(&c));
        assert_eq!(held.seq, 6);
    }

    #[test]
    fn after_a_split_the_side_with_the_higher_number_holds_and_an_item_shows_both() {
        let id7 = project(7, "x").id;
        // Side a: one edit. Side b: an unrelated edit, then the same setting, later.
        let a = numbered(&base(), &set_project(7, "from a"), id(A));
        let b = numbered(&base(), &Edit::RemoveLimits { machine: id(B) }, id(B));
        let b = numbered(&b, &set_project(7, "from b"), id(B));
        let out = merged(&a, &b);
        assert_eq!(project_named(&out, 7).as_deref(), Some("from b"));
        assert_eq!(
            out.overwritten[&SettingKey::Project(id7)],
            Overwritten {
                kept: Some(SettingValue::Project(project(7, "from b"))),
                lost: Some(SettingValue::Project(project(7, "from a"))),
            }
        );
        assert_eq!(
            out.stamps[&StampKey::Overwritten(SettingKey::Project(id7))],
            Stamp { seq: 3, by: id(B) }
        );
    }

    #[test]
    fn an_equal_number_goes_to_the_lower_machine_id() {
        let a = numbered(&base(), &set_project(7, "from a"), id(A));
        let b = numbered(&base(), &set_project(7, "from b"), id(B));
        let out = merged(&a, &b);
        assert_eq!(project_named(&out, 7).as_deref(), Some("from a"));
        assert_eq!(out.overwritten.len(), 1);
    }

    #[test]
    fn edits_to_different_settings_on_each_side_both_hold_with_no_item() {
        let a = numbered(&base(), &set_project(7, "from a"), id(A));
        let b = numbered(
            &base(),
            &Edit::SetLimits {
                machine: id(B),
                limits: limits(2),
            },
            id(B),
        );
        let out = merged(&a, &b);
        assert_eq!(project_named(&out, 7).as_deref(), Some("from a"));
        assert_eq!(out.limits[&id(B)], limits(2));
        assert!(out.overwritten.is_empty());
        assert_eq!(out.seen, [(id(A), 2), (id(B), 2)].into());
        // The result is neither input, so it takes a number above both.
        assert_eq!(out.seq, 3);
    }

    #[test]
    fn a_takeover_that_edits_what_the_old_coordinator_edited_is_no_conflict() {
        let old = numbered(&base(), &set_project(7, "by a"), id(A));
        // B took over holding a's copy, and edits the same project.
        let new = numbered(&old, &set_project(7, "by b"), id(B));
        let out = merged(&old, &new);
        assert_eq!(project_named(&out, 7).as_deref(), Some("by b"));
        assert!(out.overwritten.is_empty());
        assert_eq!(out, new);
    }

    #[test]
    fn a_machine_that_was_only_behind_takes_the_newer_copy_unchanged() {
        let behind = base();
        let ahead = numbered(&base(), &set_project(7, "p"), id(A));
        let ahead = numbered(&ahead, &set_project(7, "q"), id(A));
        let out = merged(&behind, &ahead);
        assert_eq!(out, ahead);
    }

    #[test]
    fn a_removal_on_one_side_holds_unless_the_other_side_edited_it_later() {
        let synced = numbered(&base(), &set_project(7, "p"), id(A));
        let id7 = project(7, "p").id;
        let removed = numbered(&synced, &Edit::RemoveProject { id: id7 }, id(A));
        // The other side made no edit to it.
        let other = numbered(&synced, &Edit::RemoveLimits { machine: id(B) }, id(B));
        let out = merged(&removed, &other);
        assert_eq!(project_named(&out, 7), None);
        assert!(out.overwritten.is_empty());
        // The other side edited it later: that edit holds, and both values show.
        let other = numbered(&other, &set_project(7, "kept"), id(B));
        let out = merged(&removed, &other);
        assert_eq!(project_named(&out, 7).as_deref(), Some("kept"));
        assert_eq!(
            out.overwritten[&SettingKey::Project(id7)],
            Overwritten {
                kept: Some(SettingValue::Project(project(7, "kept"))),
                lost: None,
            }
        );
    }

    #[test]
    fn a_dismissed_item_stays_dismissed_when_it_meets_a_copy_that_still_holds_it() {
        let a = numbered(&base(), &set_project(7, "from a"), id(A));
        let b = numbered(&base(), &set_project(7, "from b"), id(B));
        let joined = merged(&a, &b);
        let key = SettingKey::Project(project(7, "p").id);
        assert!(joined.overwritten.contains_key(&key));
        let dismissed = numbered(
            &joined,
            &Edit::Dismiss {
                setting: key.clone(),
            },
            id(A),
        );
        assert!(dismissed.overwritten.is_empty());
        let out = merged(&dismissed, &joined);
        assert!(out.overwritten.is_empty());
        assert_eq!(out, dismissed);
        // So does a side that held only one of the split's copies.
        let out = merged(&dismissed, &b);
        assert!(out.overwritten.is_empty());
        assert_eq!(project_named(&out, 7).as_deref(), Some("from a"));
    }

    #[test]
    fn copies_that_differ_only_in_the_order_of_their_lists_merge_to_one_order() {
        let mut a = numbered(&base(), &set_project(9, "nine"), id(A));
        a = numbered(&a, &set_project(8, "eight"), id(A));
        let mut b = a.clone();
        b.projects.reverse();
        b.machines.reverse();
        assert_eq!(merge(&a, &b).projects, merge(&b, &a).projects);
        assert_eq!(merge(&a, &b).machines, merge(&b, &a).machines);
    }

    #[test]
    fn the_merged_seq_is_the_higher_one_when_the_result_is_that_copy() {
        let a = numbered(&base(), &set_project(7, "p"), id(A));
        let mut same = a.clone();
        same.seq = 9;
        let out = merged(&a, &same);
        assert_eq!(out.seq, 9);
        // A copy with a higher seq that the merge changes is passed by one.
        let mut stale = base();
        stale.seq = 9;
        let out = merged(&a, &stale);
        assert_eq!(project_named(&out, 7).as_deref(), Some("p"));
        assert_eq!(out.seq, 10);
    }

    #[test]
    fn copies_with_no_stamps_merge_with_no_item() {
        let mut a = base();
        a.projects = vec![project(7, "a")];
        let mut b = base();
        b.projects = vec![project(7, "b")];
        let out = merged(&a, &b);
        assert!(out.overwritten.is_empty());
        assert_eq!(out.projects.len(), 1);
    }
}
