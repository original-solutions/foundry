//! Listing a machine that joined with a code.

#[cfg(test)]
mod tests {
    use std::io;
    use std::net::IpAddr;
    use std::sync::{Arc, Mutex};

    use foundry_proto::ids::MachineId;
    use foundry_proto::settings::{Machine, SharedSettings};
    use futures::future::BoxFuture;

    use super::*;
    use crate::coordinator::Coordinator;
    use crate::store::Store;
    use crate::sync::{Conn, Dialer, Peers};

    struct NoDial;

    impl Dialer for NoDial {
        fn dial(&self, _: &Machine) -> BoxFuture<'static, io::Result<Conn>> {
            Box::pin(async { Err(io::Error::other("no peers in these tests")) })
        }
    }

    /// A machine listed as `personal-mac` that acts alone, holding `others` beside itself, and
    /// its lister.
    struct Listed {
        _data: tempfile::TempDir,
        store: Arc<Mutex<Store>>,
        lister: Lister,
    }

    impl Listed {
        fn new(others: &[Machine]) -> Self {
            let data = tempfile::tempdir().unwrap();
            let me = MachineId::random();
            let mut store = Store::open(data.path().join("foundry.db")).unwrap();
            let mut held = store.settings().unwrap();
            held.seq += 1;
            held.machines
                .push(machine(me, "personal-mac", "100.64.0.1"));
            held.machines.extend(others.iter().cloned());
            held.priority = vec![me];
            store.apply_settings(held).unwrap();
            let store = Arc::new(Mutex::new(store));
            let peers = Peers::new(me, store.clone(), Arc::new(NoDial));
            let coordinator = Arc::new(Coordinator::new(me, store.clone(), peers.clone()));
            peers.coordinate(coordinator.clone());
            let lister = Lister::new(coordinator, store.clone());
            Self {
                _data: data,
                store,
                lister,
            }
        }

        fn settings(&self) -> SharedSettings {
            self.store.lock().unwrap().settings().unwrap()
        }

        fn held(&self, id: MachineId) -> Option<Machine> {
            self.settings().machines.into_iter().find(|m| m.id == id)
        }
    }

    fn machine(id: MachineId, name: &str, address: &str) -> Machine {
        Machine {
            id,
            name: name.into(),
            address: address.into(),
            key: String::new(),
        }
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[tokio::test]
    async fn a_machine_the_list_does_not_hold_is_listed_as_it_comes() {
        let listed = Listed::new(&[]);
        let joining = Machine {
            key: "ab".repeat(32),
            ..machine(MachineId::random(), "trilogy-mac", "100.64.0.2")
        };

        listed.lister.list(joining.clone()).await.unwrap();

        assert_eq!(listed.held(joining.id), Some(joining));
    }

    #[tokio::test]
    async fn a_machine_the_list_holds_is_left_as_it_is() {
        let id = MachineId::random();
        let held = machine(id, "trilogy-mac", "100.64.0.2");
        let listed = Listed::new(std::slice::from_ref(&held));
        let seq = listed.settings().seq;

        listed
            .lister
            .list(machine(id, "renamed-mac", "100.64.0.9"))
            .await
            .unwrap();

        assert_eq!(listed.held(id), Some(held));
        assert_eq!(listed.settings().seq, seq, "nothing was sent");
    }

    #[tokio::test]
    async fn a_name_another_machine_has_in_any_case_or_spacing_is_refused_naming_it() {
        let other = machine(MachineId::random(), "Trilogy-Mac", "100.64.0.2");
        let listed = Listed::new(std::slice::from_ref(&other));
        let joining = machine(MachineId::random(), " trilogy-mac ", "100.64.0.3");

        let why = listed.lister.list(joining.clone()).await.unwrap_err();

        assert!(why.contains("\"Trilogy-Mac\" at 100.64.0.2"), "{why}");
        assert!(why.contains("rename that machine first"), "{why}");
        assert_eq!(listed.held(joining.id), None);
    }

    #[tokio::test]
    async fn an_address_another_machine_has_is_refused_naming_it() {
        let other = machine(MachineId::random(), "trilogy-mac", " fd7a:115c:a1e0:0::2 ");
        let listed = Listed::new(std::slice::from_ref(&other));
        let joining = machine(MachineId::random(), "studio-mac", "fd7a:115c:a1e0::2");

        let why = listed.lister.list(joining.clone()).await.unwrap_err();

        assert!(why.contains("\"trilogy-mac\""), "{why}");
        assert!(why.contains("cannot join at that address"), "{why}");
        assert_eq!(listed.held(joining.id), None);
    }

    #[test]
    fn a_listed_address_is_compared_as_an_ip_when_it_parses_and_as_text_when_not() {
        assert!(has_address(" 100.64.0.2 ", ip("100.64.0.2")));
        assert!(has_address("fd7a:115c:a1e0:0::2", ip("fd7a:115c:a1e0::2")));
        assert!(!has_address("100.64.0.3", ip("100.64.0.2")));
        assert!(!has_address("trilogy-mac", ip("100.64.0.2")));
    }
}
