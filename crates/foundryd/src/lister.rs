//! Listing a machine that used a join code the add route (`POST /api/machines/add`) made, for
//! the join route ([`crate::join::router`]): through the acting coordinator, as every machine
//! list change is, and only when no other machine has its name or its address. It sits apart
//! from the API so the join route lists through it without depending on the API layer.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use foundry_proto::settings::{Edit, Machine};

use crate::coordinator::Coordinator;
use crate::store::Store;
use crate::sync::on_store;

/// Lists a machine that used a code the add route made, for the join route.
#[derive(Clone)]
pub struct Lister {
    coordinator: Arc<Coordinator>,
    store: Arc<Mutex<Store>>,
}

impl Lister {
    pub fn new(coordinator: Arc<Coordinator>, store: Arc<Mutex<Store>>) -> Self {
        Self { coordinator, store }
    }

    /// Lists `machine`, which used a code, and says once the edit is numbered. A list that
    /// already holds it (this machine included) is left as it is: its entry is never
    /// overwritten, and a join sent again after a lost answer finds it there. A list that
    /// cannot be read, another machine with its name or its address, or an edit that fails
    /// gives why, which is warned. The code is never logged.
    pub(crate) async fn list(&self, machine: Machine) -> Result<(), String> {
        let id = machine.id;
        let held = on_store(&self.store, |store| store.settings())
            .await
            .map_err(|error| format!("the machine list could not be read: {error}"))?;
        if held.machines.iter().any(|m| m.id == id) {
            return Ok(());
        }
        let wanted = machine.name.trim().to_lowercase();
        let address = machine.address.trim();
        let at_address = |m: &Machine| {
            address
                .parse::<IpAddr>()
                .map_or(m.address.trim() == address, |ip| {
                    has_address(&m.address, ip)
                })
        };
        if let Some(other) = held
            .machines
            .iter()
            .find(|m| at_address(m) || m.name.trim().to_lowercase() == wanted)
        {
            // A rename keeps the address, and no machine can be removed yet.
            let way_out = if at_address(other) {
                "a machine cannot be removed from the list yet, so this one cannot join at that \
                 address"
            } else {
                "rename that machine first"
            };
            let why = format!(
                "{:?} at {} is already in the machine list, so the machine is not listed as {:?} \
                 at {}; {way_out}",
                other.name, other.address, machine.name, machine.address
            );
            tracing::warn!("machine {id} used a code to join, but {why}");
            return Err(why);
        }
        match self.coordinator.edit(Edit::SetMachine { machine }).await {
            Ok(_) => Ok(()),
            Err(error) => {
                tracing::warn!(
                    "machine {id} used a code to join, but it could not be listed: {error}"
                );
                Err(error.to_string())
            }
        }
    }
}

/// Whether a listed `address` is `ip`: compared as an IP when it parses as one, else as text.
pub(crate) fn has_address(address: &str, ip: IpAddr) -> bool {
    let address = address.trim();
    address
        .parse::<IpAddr>()
        .map_or_else(|_| address == ip.to_string(), |listed| listed == ip)
}

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
    use crate::test_log::Logged;

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
        let joining = machine(MachineId::random(), " TRILOGY-mac ", "100.64.0.3");
        let logged = Logged::default();
        let _capture = logged.capture();

        let why = listed.lister.list(joining.clone()).await.unwrap_err();

        assert!(why.contains("\"Trilogy-Mac\" at 100.64.0.2"), "{why}");
        assert!(why.contains("rename that machine first"), "{why}");
        assert_eq!(listed.held(joining.id), None);
        let warnings = logged.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains(&joining.id.to_string()),
            "{warnings:?}"
        );
        assert!(warnings[0].contains("Trilogy-Mac"), "{warnings:?}");
    }

    #[tokio::test]
    async fn an_address_another_machine_has_is_refused_naming_it() {
        let other = machine(MachineId::random(), "trilogy-mac", " fd7a:115c:a1e0:0::2 ");
        let listed = Listed::new(std::slice::from_ref(&other));
        let joining = machine(MachineId::random(), "studio-mac", "fd7a:115c:a1e0::2");
        let logged = Logged::default();
        let _capture = logged.capture();

        let why = listed.lister.list(joining.clone()).await.unwrap_err();

        assert!(why.contains("\"trilogy-mac\""), "{why}");
        assert!(why.contains("cannot join at that address"), "{why}");
        assert_eq!(listed.held(joining.id), None);
        let warnings = logged.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains(&joining.id.to_string()),
            "{warnings:?}"
        );
        assert!(warnings[0].contains("trilogy-mac"), "{warnings:?}");
    }

    #[test]
    fn a_listed_address_is_compared_as_an_ip_when_it_parses_and_as_text_when_not() {
        assert!(has_address(" 100.64.0.2 ", ip("100.64.0.2")));
        assert!(has_address("fd7a:115c:a1e0:0::2", ip("fd7a:115c:a1e0::2")));
        assert!(!has_address("100.64.0.3", ip("100.64.0.2")));
        assert!(!has_address("trilogy-mac", ip("100.64.0.2")));
    }
}
