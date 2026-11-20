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

use std::collections::{BTreeMap, BTreeSet};

use foundry_proto::ids::MachineId;
use foundry_proto::settings::{Edit, Overwritten, SettingKey, SettingValue, SharedSettings, Stamp};
use serde::Serialize;

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
/// of each. The result's `seq` is the higher of the two, plus one when the result is not the
/// copy that holds it, so a copy that changed is always taken by a store holding either.
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
        let conflict =
            !key.is_overwritten() && held != lost && !covers(&a.seen, sb) && !covers(&b.seen, sa);
        if conflict {
            let item = Overwritten {
                kept: setting(&held),
                lost: setting(&lost),
            };
            conflicts.push((key.clone(), item, stamp));
        }
        put(&mut out, key, held, stamp);
    }
    for (key, item, stamp) in conflicts {
        let key = SettingKey::Overwritten(Box::new(key));
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
fn keys(settings: &SharedSettings) -> BTreeSet<SettingKey> {
    let mut keys: BTreeSet<SettingKey> = settings.stamps.keys().cloned().collect();
    keys.extend(settings.machines.iter().map(|m| SettingKey::Machine(m.id)));
    keys.extend(settings.projects.iter().map(|p| SettingKey::Project(p.id)));
    keys.extend(settings.limits.keys().map(|id| SettingKey::Limits(*id)));
    keys.extend(
        settings
            .overwritten
            .keys()
            .map(|key| SettingKey::Overwritten(Box::new(key.clone()))),
    );
    if !settings.priority.is_empty() {
        keys.insert(SettingKey::Priority);
    }
    keys
}

fn stamp(settings: &SharedSettings, key: &SettingKey) -> Stamp {
    settings.stamps.get(key).copied().unwrap_or(Stamp::NONE)
}

/// The value `settings` holds for `key`, `None` when it holds none. An empty priority order is
/// none.
fn get(settings: &SharedSettings, key: &SettingKey) -> Option<Value> {
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
        SettingKey::Overwritten(inner) => {
            return settings.overwritten.get(inner).cloned().map(Value::Item);
        }
    };
    value.map(Value::Setting)
}

/// Sets `key` to `value` (`None` removes it) with `stamp`. Machines and projects are kept in
/// id order, so two machines holding the same settings hold the same lists. A value of the
/// wrong kind for `key` is taken as none.
fn put(settings: &mut SharedSettings, key: &SettingKey, value: Option<Value>, stamp: Stamp) {
    match key {
        SettingKey::Machine(id) => {
            settings.machines.retain(|m| m.id != *id);
            if let Some(Value::Setting(SettingValue::Machine(m))) = value
                && m.id == *id
            {
                settings.machines.push(m);
                settings.machines.sort_by_key(|m| m.id);
            }
        }
        SettingKey::Priority => {
            settings.priority = match value {
                Some(Value::Setting(SettingValue::Priority(order))) => order,
                _ => Vec::new(),
            };
        }
        SettingKey::Project(id) => {
            settings.projects.retain(|p| p.id != *id);
            if let Some(Value::Setting(SettingValue::Project(p))) = value
                && p.id == *id
            {
                settings.projects.push(p);
                settings.projects.sort_by_key(|p| p.id);
            }
        }
        SettingKey::Limits(id) => match value {
            Some(Value::Setting(SettingValue::Limits(limits))) => {
                settings.limits.insert(*id, limits);
            }
            _ => {
                settings.limits.remove(id);
            }
        },
        SettingKey::Overwritten(inner) => match value {
            Some(Value::Item(item)) => {
                settings.overwritten.insert((**inner).clone(), item);
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
        Stamp,
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
            next.stamps[&SettingKey::Project(project(7, "p").id)],
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
            next.stamps[&SettingKey::Project(id7)],
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
            out.stamps[&SettingKey::Overwritten(Box::new(SettingKey::Project(id7)))],
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
