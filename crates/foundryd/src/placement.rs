//! Picks a machine for a workspace or gives the reasons none fits. Filled by plan task 15.

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::time::Duration;

    use foundry_proto::ids::{MachineId, ProjectId};
    use foundry_proto::record::{MachineSample, Record};
    use foundry_proto::settings::{Limits, SharedSettings};

    use super::*;
    use crate::attention::{Fleet, Kind, Source as _};

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
        let [a, b, c] = [MachineId::random(), MachineId::random(), MachineId::random()];
        let samples = [sample(a, 1, 0.6), sample(b, 3, 0.2), sample(c, 0, 1.4)];
        // c has the fewest sessions but is over its load limit.
        assert_eq!(place(&samples, &limited(&[a, b, c]), &all(&[a, b, c])), Ok(b));
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
        let [mac, vps, mini] = [MachineId::random(), MachineId::random(), MachineId::random()];
        let names = BTreeMap::from([(mac, "trilogy-mac"), (vps, "vps"), (mini, "old-mini")]);
        let samples = [sample(mac, 1, 1.8), sample(vps, 4, 1.5), sample(mini, 0, 0.1)];
        let refused =
            place(&samples, &limited(&[mac, vps, mini]), &all(&[mac, vps])).unwrap_err();
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
        let id = queue.wait(want("fix login"), vec!["studio: load 1.8 over limit 1.0".into()]);
        assert!(queue.items(&fleet(me)).is_empty());
        tokio::time::advance(Duration::from_secs(4 * 60 + 59)).await;
        assert!(queue.items(&fleet(me)).is_empty(), "raised before 5 minutes");
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
        // A placement that could not reach its machine waits again, and can be cancelled.
        queue.finish(placing, Outcome::Retry);
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

    #[test]
    fn stopping_drops_every_request() {
        let queue = Queue::new(MachineId::random());
        queue.wait(want("a"), Vec::new());
        queue.wait(want("b"), Vec::new());
        assert_eq!(queue.drop_all(), 2);
        assert!(queue.list().is_empty());
    }
}
