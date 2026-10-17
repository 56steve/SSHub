//! A move must clear the moved hosts'/identities' saved secrets from the
//! SOURCE credential store; a copy must delete nothing; failures are reported.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use anyhow::Result;
use sshub::credentials::{host_key, identity_key, PasswordStore};
use sshub::store::transfer::{
    apply_transfer_plan, build_transfer_plan, secret_purge_warning, DestIndex, GroupTransferMode,
    TransferMode, TransferSelection, TransferSnapshot,
};
use sshub::store::{LauncherStore, NewHost, NewIdentity};

/// In-memory credential store; keys in `fail` refuse deletion.
#[derive(Default)]
struct FakeSecrets {
    map: Mutex<HashMap<String, String>>,
    fail: HashSet<String>,
}

impl PasswordStore for FakeSecrets {
    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.map.lock().unwrap().get(key).cloned())
    }
    fn set(&self, key: &str, password: &str) -> Result<()> {
        self.map.lock().unwrap().insert(key.into(), password.into());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<()> {
        if self.fail.contains(key) {
            anyhow::bail!("keyring locked");
        }
        self.map.lock().unwrap().remove(key);
        Ok(())
    }
}

fn run(
    src: &mut LauncherStore,
    mode: TransferMode,
    hosts: &[&str],
) -> sshub::store::TransferResult {
    let mut dest = LauncherStore::open_in_memory().unwrap();
    let plan = build_transfer_plan(
        &TransferSnapshot::capture(src).unwrap(),
        &DestIndex::capture(&dest, "dest").unwrap(),
        &TransferSelection {
            host_names: hosts.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        },
        mode,
        GroupTransferMode::GroupOnly,
        &HashSet::new(),
    );
    apply_transfer_plan(src, &mut dest, &plan, mode).unwrap()
}

fn host(store: &LauncherStore, name: &str, identity_id: Option<i64>) -> i64 {
    store
        .create_host(&NewHost {
            identity_id,
            address: format!("{name}.example.com"),
            ..NewHost::launcher(name, "")
        })
        .unwrap()
        .id
}

#[test]
fn move_purges_host_and_orphaned_identity_secrets_but_keeps_the_rest() {
    let mut src = LauncherStore::open_in_memory().unwrap();
    let ident = src
        .create_identity(&NewIdentity {
            name: "solo".into(),
            username: None,
            private_key: None,
            certificate: None,
            sort_order: 0,
            has_password: true,
        })
        .unwrap()
        .id;
    let moved = host(&src, "moved", Some(ident));
    let stay = host(&src, "stay", None);
    let secrets = FakeSecrets::default();
    for k in [host_key(moved), host_key(stay), identity_key(ident)] {
        secrets.set(&k, "pw").unwrap();
    }

    let result = run(&mut src, TransferMode::Move, &["moved"]);
    assert!(result.purge_source_secrets(&secrets).is_empty());

    assert_eq!(secrets.get(&host_key(moved)).unwrap(), None);
    assert_eq!(secrets.get(&identity_key(ident)).unwrap(), None);
    assert_eq!(secrets.get(&host_key(stay)).unwrap().as_deref(), Some("pw"));
}

#[test]
fn move_keeps_secret_of_identity_still_used_by_source_host() {
    let mut src = LauncherStore::open_in_memory().unwrap();
    let ident = src
        .create_identity(&NewIdentity {
            name: "shared".into(),
            username: None,
            private_key: None,
            certificate: None,
            sort_order: 0,
            has_password: true,
        })
        .unwrap()
        .id;
    host(&src, "moved", Some(ident));
    host(&src, "stay", Some(ident));
    let secrets = FakeSecrets::default();
    secrets.set(&identity_key(ident), "pw").unwrap();

    let result = run(&mut src, TransferMode::Move, &["moved"]);
    assert!(result.purge_source_secrets(&secrets).is_empty());
    assert!(secrets.get(&identity_key(ident)).unwrap().is_some());
}

#[test]
fn copy_deletes_no_secrets() {
    let mut src = LauncherStore::open_in_memory().unwrap();
    let id = host(&src, "web", None);
    let secrets = FakeSecrets::default();
    secrets.set(&host_key(id), "pw").unwrap();

    let result = run(&mut src, TransferMode::Copy, &["web"]);
    assert!(result.deleted_host_ids.is_empty());
    assert!(result.purge_source_secrets(&secrets).is_empty());
    assert!(secrets.get(&host_key(id)).unwrap().is_some());
}

#[test]
fn purge_failures_are_reported_not_swallowed() {
    let mut src = LauncherStore::open_in_memory().unwrap();
    let a = host(&src, "a", None);
    let b = host(&src, "b", None);
    let mut secrets = FakeSecrets::default();
    secrets.fail.insert(host_key(a));
    secrets.set(&host_key(a), "pw").unwrap();
    secrets.set(&host_key(b), "pw").unwrap();

    let result = run(&mut src, TransferMode::Move, &["a", "b"]);
    let failures = result.purge_source_secrets(&secrets);

    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains(&host_key(a)));
    // The healthy key is still purged despite the other failing.
    assert_eq!(secrets.get(&host_key(b)).unwrap(), None);
    assert!(secret_purge_warning(&failures).starts_with("1 moved secret(s)"));
}
