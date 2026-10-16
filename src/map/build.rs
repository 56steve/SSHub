//! Collect [`MapHost`]s from the launcher store and ssh_config.

use std::collections::HashMap;

use anyhow::Result;

use super::{JumpSource, MapHost};
use crate::app::HostEntry;
use crate::store::{HostGroup, HostSource, LauncherStore};

fn group_path(groups: &HashMap<i64, HostGroup>, id: Option<i64>) -> String {
    let mut names = Vec::new();
    let mut next = id;
    while let Some(g) = next.and_then(|i| groups.get(&i)) {
        names.push(g.name.as_str());
        next = g.parent_id;
        if names.len() > 32 {
            break;
        }
    }
    if names.is_empty() {
        "_ungrouped".into()
    } else {
        names.reverse();
        names.join("/")
    }
}

/// Every host with its effective jump, in `entries` order.
///
/// Launcher hosts use the connect walk (`resolve_connection`: own value, then
/// nearest group). ssh_config hosts connect by alias, so ssh applies
/// `~/.ssh/config` and only that counts: their stored `ProxyJump` is what
/// `ssh -G` reported at the last sync (`sync_ssh_config_hosts` runs on every
/// reload), and `ProxyCommand` presence rides in the same sync fingerprint.
/// Neither costs an extra `ssh -G` here. A group default never applies to them.
/// Each launcher host costs a few SQLite reads; rebuild on change, not per frame.
pub fn collect_hosts(store: &LauncherStore, entries: &[HostEntry]) -> Result<Vec<MapHost>> {
    let groups: HashMap<i64, HostGroup> = store
        .list_groups()?
        .into_iter()
        .map(|g| (g.id, g))
        .collect();
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        out.push(match entry {
            HostEntry::Legacy { host, meta } => MapHost {
                name: host.name.clone(),
                address: host.hostname.clone().unwrap_or_else(|| host.name.clone()),
                user: host.user.clone(),
                port: host.port.unwrap_or(22),
                group: "_ungrouped".into(),
                tags: meta.tags.clone(),
                jump: non_empty(host.proxy_jump.clone()),
                jump_source: host.proxy_jump.as_ref().map(|_| JumpSource::SshConfig),
                ssh_config: true,
                proxy_command: host.has_proxy_command,
            },
            HostEntry::Managed(m) => {
                // A stored jump ssh must not parse (leading `-`) makes
                // resolution fail; the map still shows the host, with the
                // hop as invalid.
                let resolved = store
                    .resolve_connection(m)
                    .unwrap_or_else(|_| crate::store::ResolvedConnection::from_stored(m));
                let user = resolved
                    .username
                    .clone()
                    .or_else(|| resolved.identity.as_ref().and_then(|i| i.username.clone()));
                let ssh_config = m.source == HostSource::SshConfig;
                let (jump, jump_source, proxy_command) = if ssh_config {
                    let jump = non_empty(m.proxy_jump.clone());
                    let source = jump.as_ref().map(|_| JumpSource::SshConfig);
                    let pc = m
                        .ssh_config_hash
                        .as_deref()
                        .is_some_and(crate::ssh::ssh_config_hash_has_proxy_command);
                    (jump, source, pc)
                } else {
                    let jump = non_empty(resolved.proxy_jump.clone());
                    let source = match (&jump, &m.proxy_jump) {
                        (None, _) => None,
                        (Some(_), Some(_)) => Some(JumpSource::Own),
                        (Some(_), None) => Some(
                            store
                                .proxy_jump_group(m)?
                                .map(|g| JumpSource::Inherited {
                                    group: group_path(&groups, Some(g.id)),
                                })
                                .unwrap_or(JumpSource::Own),
                        ),
                    };
                    (jump, source, false)
                };
                MapHost {
                    name: m.name.clone(),
                    address: m.address.clone(),
                    user,
                    port: resolved.port,
                    group: group_path(&groups, m.group_id),
                    tags: m.tags.clone(),
                    jump,
                    jump_source,
                    ssh_config,
                    proxy_command,
                }
            }
        });
    }
    Ok(out)
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::MetadataDb;
    use crate::ssh::{import_ssh_config, SshConfigResolver};

    /// Oracle: for every ssh_config host, the jump and ProxyCommand presence the
    /// map reads (after a real sync through `ssh -G`) must equal what a raw
    /// `ssh -G <host>` prints, including wildcard `Host` blocks and `none`.
    #[test]
    fn ssh_config_hosts_match_raw_ssh_g() {
        if std::process::Command::new("ssh")
            .arg("-V")
            .output()
            .is_err()
        {
            eprintln!("skipping: no ssh binary");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config");
        std::fs::write(
            &cfg,
            "Host db-*\n  ProxyJump bastion\n\
             Host bastion\n  HostName 10.0.0.1\n\
             Host db-1\n  HostName 10.0.1.1\n\
             Host db-2\n  HostName 10.0.1.2\n  ProxyJump none\n\
             Host db-3\n  HostName 10.0.1.3\n  ProxyJump u@h1,h2:2200\n\
             Host iot\n  HostName 10.0.2.1\n  ProxyCommand ssh -W %h:%p bastion\n\
             Host plain\n  HostName 10.0.3.1\n",
        )
        .unwrap();

        let store = LauncherStore::open_in_memory().unwrap();
        let resolver = SshConfigResolver::with_config_path(&cfg);
        let meta = MetadataDb::default();
        import_ssh_config(&resolver, &store, &meta).unwrap();
        let entries: Vec<HostEntry> = store
            .list_hosts()
            .unwrap()
            .into_iter()
            .map(HostEntry::from_managed)
            .collect();
        let hosts = collect_hosts(&store, &entries).unwrap();
        assert_eq!(hosts.len(), 6);

        for h in &hosts {
            let out = std::process::Command::new("ssh")
                .arg("-F")
                .arg(&cfg)
                .args(["-G", &h.name])
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            let jump = text
                .lines()
                .find_map(|l| l.strip_prefix("proxyjump "))
                .map(String::from);
            let pc = text.lines().any(|l| l.starts_with("proxycommand "));
            assert_eq!(h.jump, jump, "{}: jump\n{text}", h.name);
            assert_eq!(h.proxy_command, pc, "{}: proxycommand\n{text}", h.name);
            assert!(h.ssh_config);
        }
        assert!(hosts.iter().any(|h| h.name == "db-1" && h.jump.is_some()));
        assert!(hosts.iter().any(|h| h.name == "iot" && h.proxy_command));
    }

    #[test]
    fn launcher_hosts_inherit_group_jump_and_report_the_source() {
        use crate::store::{NewHost, NewHostGroup};
        let store = LauncherStore::open_in_memory().unwrap();
        let parent = store
            .create_group(&NewHostGroup {
                name: "prod".into(),
                default_proxy_jump: Some("bastion".into()),
                ..Default::default()
            })
            .unwrap();
        let child = store
            .create_group(&NewHostGroup {
                name: "db".into(),
                parent_id: Some(parent.id),
                ..Default::default()
            })
            .unwrap();
        store
            .create_host(&NewHost::launcher("bastion", "10.0.0.1"))
            .unwrap();
        let mut inherited = NewHost::launcher("db-1", "10.0.1.1");
        inherited.group_id = Some(child.id);
        store.create_host(&inherited).unwrap();
        let mut own = NewHost::launcher("db-2", "10.0.1.2");
        own.group_id = Some(child.id);
        own.proxy_jump = Some("ops@203.0.113.7".into());
        store.create_host(&own).unwrap();
        let mut hostile = NewHost::launcher("db-3", "10.0.1.3");
        hostile.proxy_jump = Some("-oProxyCommand=id".into());
        store.create_host(&hostile).unwrap();

        let entries: Vec<HostEntry> = store
            .list_hosts()
            .unwrap()
            .into_iter()
            .map(HostEntry::from_managed)
            .collect();
        let map = crate::map::Map::build(collect_hosts(&store, &entries).unwrap());
        let by = |n: &str| map.hosts.iter().find(|h| h.name == n).unwrap();
        assert_eq!(by("db-1").group, "prod/db");
        assert_eq!(by("db-1").jump.as_deref(), Some("bastion"));
        assert_eq!(
            by("db-1").jump_source,
            Some(JumpSource::Inherited {
                group: "prod".into()
            })
        );
        assert_eq!(by("db-2").jump_source, Some(JumpSource::Own));
        // The hostile token survives as an invalid hop: not a host, not raw.
        let i = map.hosts.iter().position(|h| h.name == "db-3").unwrap();
        assert!(matches!(
            &map.chains[i],
            crate::map::Chain::Hops(h) if matches!(h[0].kind, crate::map::HopKind::Invalid(_))
        ));
        // And connect refuses it: an error the app turns into a notice.
        let m = store.get_host_by_name("db-3").unwrap().unwrap();
        assert!(store.resolve_connection(&m).is_err());
    }
}
