//! Group-inherited connection defaults (issue #74).
//!
//! Groups may carry optional defaults for the six connection fields
//! (identity, username, port, ProxyJump, transport, agent forwarding).
//! Resolution order per field:
//!
//! 1. the host's own explicit value,
//! 2. the best default found on any group the host belongs to
//!    (most-specific-wins — see below),
//! 3. the global default (port 22, transport ssh, agent forwarding off;
//!    identity/username/ProxyJump stay unset).
//!
//! Most-specific-wins: every membership (the primary `group_id` plus each
//! `host_group_memberships` row) contributes its ancestor chain, nearest
//! first. Each group on those chains that sets the field is a candidate
//! carrying (chain distance from the host, group tree-depth, primary-first,
//! group id). The winner is the smallest chain distance; ties break toward
//! the deeper (more specific) group, then the primary group's chain, then
//! the smaller group id for determinism. A host sitting directly in both a
//! parent and its child therefore resolves a conflict between their
//! defaults in favour of the child (both distance 0, child deeper).
//! Single-group hosts behave exactly as before: the primary chain's nearest
//! setter wins.
//!
//! Tags/favorites never inherit (they are host/metadata scoped).

use anyhow::Result;

use super::types::{HostGroup, Identity, ManagedHost};
use super::LauncherStore;
use crate::session_transport::SessionTransport;

/// Global fallback port when neither the host nor any ancestor group sets one.
pub const DEFAULT_PORT: u16 = 22;

/// Effective connection values for one host after inheritance resolution.
/// Every field is concrete: [`LauncherStore::resolve_connection`] applies the
/// host → ancestors → global walk documented above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConnection {
    pub identity_id: Option<i64>,
    /// The effective identity (the host's own, else the nearest ancestor
    /// group's default), loaded so callers can fall back to its username/key.
    pub identity: Option<Identity>,
    /// Explicit username (host, else nearest group default). The identity's
    /// username remains a further fallback applied by the argv builder.
    pub username: Option<String>,
    pub port: u16,
    pub proxy_jump: Option<String>,
    /// [`Self::proxy_jump`] as handed to `ssh -J`: a hop naming a launcher-only
    /// host (one `ssh` cannot look up in `~/.ssh/config`) is expanded to
    /// `user@address:port`. Display, export and the form keep `proxy_jump`.
    pub jump_argv: Option<String>,
    /// Set when `proxy_jump` is not valid hops (so `jump_argv` is `None`): a
    /// caller that runs ssh must refuse with this message. Connecting without
    /// the jump would bypass the bastion the user configured.
    pub jump_error: Option<String>,
    pub transport: SessionTransport,
    pub forward_agent: bool,
}

/// The `-J` value of an unresolved jump: dropped when any hop is not a valid
/// `[user@]host[:port]`, so a stored `-o...` can never reach `ssh -J` through a
/// fallback that skipped [`LauncherStore::resolve_connection`].
fn valid_jump(jump: Option<String>) -> Option<String> {
    jump.filter(|j| crate::map::hops::jump_is_valid(j))
}

impl ResolvedConnection {
    /// The global defaults: the end of every resolution walk.
    pub fn global_default() -> Self {
        Self {
            identity_id: None,
            identity: None,
            username: None,
            port: DEFAULT_PORT,
            proxy_jump: None,
            jump_argv: None,
            jump_error: None,
            transport: SessionTransport::Ssh,
            forward_agent: false,
        }
    }

    /// The same walk with every host field unset: what the host form shows as
    /// muted placeholders ("clearing a field restores this").
    pub fn for_group_chain(chain: &[HostGroup]) -> Self {
        // `chain` is most-specific-first (nearest-first for a single group):
        // the first group that sets a field wins.
        let mut identity_id = None;
        let mut username: Option<String> = None;
        let mut port = None;
        let mut proxy_jump: Option<String> = None;
        let mut transport = None;
        let mut forward_agent = None;
        for group in chain {
            identity_id = identity_id.or(group.default_identity_id);
            username = username.or_else(|| group.default_username.clone());
            port = port.or(group.default_port);
            proxy_jump = proxy_jump.or_else(|| group.default_proxy_jump.clone());
            transport = transport.or(group.default_transport);
            forward_agent = forward_agent.or(group.default_forward_agent);
        }
        Self {
            identity_id,
            identity: None,
            username,
            port: port.unwrap_or(DEFAULT_PORT),
            jump_argv: valid_jump(proxy_jump.clone()),
            jump_error: proxy_jump
                .as_deref()
                .and_then(crate::map::hops::jump_problem),
            proxy_jump,
            transport: transport.unwrap_or_default(),
            forward_agent: forward_agent.unwrap_or(false),
        }
    }

    /// Stored host values with global fallbacks and no group walk: what
    /// display-only paths use when they cannot resolve (connect paths use
    /// [`LauncherStore::resolve_connection`]).
    pub fn from_stored(host: &ManagedHost) -> Self {
        Self {
            identity_id: host.identity_id,
            identity: host.identity.clone(),
            username: host.username.clone(),
            port: host.port.unwrap_or(DEFAULT_PORT),
            proxy_jump: host.proxy_jump.clone(),
            jump_argv: valid_jump(host.proxy_jump.clone()),
            jump_error: host.proxy_jump.as_deref().and_then(|j| {
                crate::map::hops::jump_problem(j).or_else(|| {
                    crate::map::hops::split(j)
                        .iter()
                        .any(|t| {
                            t == &host.name
                                || crate::map::parse_raw_hop(t).is_some_and(|r| r.host == host.name)
                        })
                        .then(|| format!("{} jumps through itself", host.name))
                })
            }),
            transport: host.transport.unwrap_or_default(),
            forward_agent: host.forward_agent.unwrap_or(false),
        }
    }
}

impl LauncherStore {
    /// Ancestor chain for `group_id`, nearest first (the group itself, then
    /// its parent, grandparent, …). A parent cycle cannot loop: visited ids
    /// are skipped. Missing groups end the walk.
    pub fn group_ancestor_chain(&self, group_id: Option<i64>) -> Result<Vec<HostGroup>> {
        let mut chain = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut next = group_id;
        while let Some(id) = next {
            if !seen.insert(id) {
                break;
            }
            let Some(group) = self.get_group(id)? else {
                break;
            };
            next = group.parent_id;
            chain.push(group);
        }
        Ok(chain)
    }

    /// Every default-setting group visible to `host`, ordered most-specific
    /// first: smallest ancestor-chain distance from the host, then deeper
    /// (more specific) group, then the primary group's chain, then smaller
    /// group id for determinism. The primary `group_id` seeds the member set
    /// so resolution works even when `host.groups` was not loaded; the loaded
    /// memberships (`host.groups`, via `host_group_memberships`, Favorites
    /// excluded) add the remaining chains. A single-group host yields exactly
    /// its primary chain, nearest first — the historical behavior.
    fn ordered_default_groups(&self, host: &ManagedHost) -> Result<Vec<HostGroup>> {
        let mut extra: Vec<i64> = host
            .groups
            .iter()
            .filter(|g| !g.reserved)
            .map(|g| g.id)
            .filter(|id| Some(*id) != host.group_id)
            .collect();
        extra.sort_unstable();
        extra.dedup();
        self.ordered_groups_for_members(host.group_id, &extra)
    }

    /// The [`ordered_default_groups`] walk over raw membership ids (the
    /// primary group plus every extra membership, Favorites excluded by the
    /// caller): smallest ancestor-chain distance from the host, then deeper
    /// (more specific) group, then the primary group's chain, then smaller
    /// group id for determinism.
    fn ordered_groups_for_members(
        &self,
        primary: Option<i64>,
        extra_member_ids: &[i64],
    ) -> Result<Vec<HostGroup>> {
        let mut member_ids = Vec::new();
        if let Some(p) = primary {
            member_ids.push(p);
        }
        member_ids.extend(
            extra_member_ids
                .iter()
                .copied()
                .filter(|id| Some(*id) != primary),
        );
        // (group, chain distance from the host, group tree-depth, primary chain).
        let mut candidates: Vec<(HostGroup, usize, usize, bool)> = Vec::new();
        let mut depths: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
        for member in member_ids {
            let primary_chain = Some(member) == primary;
            for (distance, group) in self
                .group_ancestor_chain(Some(member))?
                .into_iter()
                .enumerate()
            {
                if group.reserved {
                    continue;
                }
                let id = group.id;
                let depth = match depths.get(&id) {
                    Some(&d) => d,
                    None => {
                        let d = self.group_ancestor_chain(Some(id))?.len().saturating_sub(1);
                        depths.insert(id, d);
                        d
                    }
                };
                candidates.push((group, distance, depth, primary_chain));
            }
        }
        candidates.sort_by(|a, b| {
            a.1.cmp(&b.1) // chain distance: nearer wins
                .then(b.2.cmp(&a.2)) // tree-depth: deeper (more specific) wins
                .then(b.3.cmp(&a.3)) // primary group's chain wins
                .then(a.0.id.cmp(&b.0.id)) // determinism
        });
        Ok(candidates.into_iter().map(|(g, _, _, _)| g).collect())
    }

    /// Resolve the effective connection values for `host`.
    pub fn resolve_connection(&self, host: &ManagedHost) -> Result<ResolvedConnection> {
        let mut out = self.resolve_fields(host)?;
        // A host (a bastion in a group whose default jump is that bastion) must
        // not be its own hop: ssh would dial itself, and the map draws a loop.
        if let Some(jump) = out.proxy_jump.as_deref() {
            if !crate::map::is_jump_none(jump) && self.jump_names_host(jump, host.id)? {
                anyhow::bail!("{} jumps through itself", host.name);
            }
        }
        out.jump_argv = match out.proxy_jump.as_deref() {
            Some(jump) => match self.expand_jump(jump) {
                Ok(j) => {
                    out.jump_error = None;
                    Some(j)
                }
                // An ssh_config row connects by alias: ssh reads the config, the
                // stored jump is only shown, so a bad token must not stop it.
                Err(_) if host.source == crate::store::HostSource::SshConfig => {
                    Some(jump.to_string())
                }
                Err(e) => return Err(e),
            },
            None => None,
        };
        Ok(out)
    }

    /// The effective values of [`LauncherStore::resolve_connection`] except the
    /// expanded `-J` string (left equal to the stored one).
    pub fn resolve_fields(&self, host: &ManagedHost) -> Result<ResolvedConnection> {
        let chain = self.ordered_default_groups(host)?;
        let mut out = ResolvedConnection::for_group_chain(&chain);

        if host.identity_id.is_some() {
            out.identity_id = host.identity_id;
            out.identity = host.identity.clone();
        } else if let Some(id) = out.identity_id {
            out.identity = self.get_identity(id)?;
        }
        if host.username.is_some() {
            out.username = host.username.clone();
        }
        if let Some(p) = host.port {
            out.port = p;
        }
        if host.proxy_jump.is_some() {
            out.proxy_jump = host.proxy_jump.clone();
            out.jump_argv = valid_jump(out.proxy_jump.clone());
            out.jump_error = out
                .proxy_jump
                .as_deref()
                .and_then(crate::map::hops::jump_problem);
        }
        if let Some(t) = host.transport {
            out.transport = t;
        }
        if let Some(f) = host.forward_agent {
            out.forward_agent = f;
        }
        Ok(out)
    }

    /// The saved host a hop token names: the token as a name, else its host part.
    fn hop_host(&self, token: &str) -> Result<Option<ManagedHost>> {
        let Some(raw) = crate::map::parse_raw_hop(token) else {
            return Ok(None);
        };
        match self.get_host_by_name(token)? {
            Some(h) => Ok(Some(h)),
            None if !raw.ipv6 => self.get_host_by_name(&raw.host),
            None => Ok(None),
        }
    }

    /// Some hop of `jump` is the saved host `id`.
    fn jump_names_host(&self, jump: &str, id: i64) -> Result<bool> {
        for token in jump.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            if self.hop_host(token)?.is_some_and(|h| h.id == id) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Rewrite hops of `jump` that name a launcher-only host as
    /// `user@address[:port]`, the form `ssh -J` takes without a config entry.
    /// ssh_config rows and unknown tokens stay as written, since `ssh`
    /// resolves those itself (including a first hop's own ssh_config chain). A
    /// launcher-only hop's own jump setting is never followed: ssh cannot see
    /// it, and every hop is pinned by this list. Hop identities cannot ride in
    /// `-J` either: ssh uses its defaults for every hop.
    ///
    /// Every token must parse as `[user@]host[:port]` (leading `-`, whitespace
    /// and control characters are refused): the string reaches `ssh -J`
    /// verbatim and imports write it, so it is untrusted.
    pub fn expand_jump(&self, jump: &str) -> Result<String> {
        if crate::map::is_jump_none(jump) {
            return Ok(jump.trim().to_string());
        }
        let mut out: Vec<String> = Vec::new();
        for token in jump.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            let Some(raw) = crate::map::parse_raw_hop(token) else {
                anyhow::bail!("jump hop {token:?} is not a valid [user@]host[:port]");
            };
            let hop = match self.hop_host(token)? {
                Some(h) if h.source == crate::store::HostSource::Launcher => h,
                _ => {
                    out.push(token.to_string());
                    continue;
                }
            };
            let r = self.resolve_fields(&hop)?;
            let user = raw
                .user
                .clone()
                .or(r.username)
                .or_else(|| r.identity.and_then(|i| i.username));
            let port = raw.port.unwrap_or(r.port);
            let label = crate::map::RawHop {
                user,
                ipv6: hop.address.contains(':'),
                host: hop.address.clone(),
                port: (port != DEFAULT_PORT || hop.address.contains(':')).then_some(port),
            }
            .label();
            // The address and user come from the database, which imports write
            // verbatim: a comma would add a second hop, whitespace or a shell
            // character is no host. Sending the hop's NAME instead would let ssh
            // resolve it through DNS or ssh_config to a different machine than
            // the saved address, so the host is refused.
            if crate::map::hops::validate_raw(&label).is_err() {
                anyhow::bail!("jump host {token:?} has an address or user that is not a valid hop");
            }
            out.push(label);
        }
        Ok(out.join(","))
    }

    /// The group whose default ProxyJump `host` inherits, `None` when the host
    /// sets its own or no group sets one. Same winner as
    /// [`LauncherStore::resolve_connection`].
    pub fn proxy_jump_group(&self, host: &ManagedHost) -> Result<Option<HostGroup>> {
        if host.proxy_jump.is_some() {
            return Ok(None);
        }
        Ok(self
            .ordered_default_groups(host)?
            .into_iter()
            .find(|g| g.default_proxy_jump.is_some()))
    }

    /// The group a host in `member_ids` inherits its ProxyJump from, if any
    /// group sets one: the same most-specific-wins winner as
    /// [`LauncherStore::inherited_for_groups`].
    pub fn jump_group_for(
        &self,
        primary: Option<i64>,
        member_ids: &[i64],
    ) -> Result<Option<HostGroup>> {
        Ok(self
            .ordered_groups_for_members(primary, member_ids)?
            .into_iter()
            .find(|g| g.default_proxy_jump.is_some()))
    }

    /// Chain-only resolution across every group in `member_ids` (host fields
    /// unset): the inherited values the host form renders as muted
    /// placeholders when the form selects several groups. Uses the same
    /// most-specific-wins ordering as [`LauncherStore::resolve_connection`],
    /// so a cleared field restores exactly what the placeholder shows.
    pub fn inherited_for_groups(
        &self,
        primary: Option<i64>,
        member_ids: &[i64],
    ) -> Result<ResolvedConnection> {
        let chain = self.ordered_groups_for_members(primary, member_ids)?;
        let mut out = ResolvedConnection::for_group_chain(&chain);
        if let Some(id) = out.identity_id {
            out.identity = self.get_identity(id)?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{HostGroupUpdate, HostUpdate, NewHost, NewHostGroup, NewIdentity};

    fn memory() -> LauncherStore {
        LauncherStore::open_in_memory().unwrap()
    }

    fn identity(store: &LauncherStore, name: &str, username: &str) -> i64 {
        store
            .create_identity(&NewIdentity {
                name: name.into(),
                username: Some(username.into()),
                ..Default::default()
            })
            .unwrap()
            .id
    }

    /// A group with a distinctive default for every one of the six fields.
    fn full_group(store: &LauncherStore, name: &str, parent_id: Option<i64>) -> HostGroup {
        let iid = identity(store, &format!("{name}-ident"), &format!("{name}-user"));
        store
            .create_group(&NewHostGroup {
                name: name.into(),
                default_identity_id: Some(iid),
                default_username: Some(format!("{name}-login")),
                default_port: Some(2200),
                default_proxy_jump: Some(format!("{name}-bastion")),
                default_transport: Some(SessionTransport::Mosh),
                default_forward_agent: Some(true),
                parent_id,
                ..Default::default()
            })
            .unwrap()
    }

    fn host_in(store: &LauncherStore, name: &str, group: &HostGroup) -> ManagedHost {
        let mut nh = NewHost::launcher(name, "10.0.0.1");
        nh.group_id = Some(group.id);
        nh.port = None;
        nh.forward_agent = None;
        nh.transport = None;
        let created = store.create_host(&nh).unwrap();
        store.set_host_groups(created.id, &[group.id]).unwrap();
        store.get_host(created.id).unwrap().unwrap()
    }

    #[test]
    fn unresolved_fallbacks_never_carry_an_invalid_hop_into_the_jump_argv() {
        // `from_stored` and the group walk are what tunnels, broadcast, SFTP,
        // the CLI and export fall back to when resolving fails.
        let store = memory();
        let mut nh = NewHost::launcher("h", "10.0.0.1");
        nh.proxy_jump = Some("good,-oProxyCommand=id".into());
        let host = store.create_host(&nh).unwrap();
        let r = ResolvedConnection::from_stored(&host);
        assert_eq!(r.jump_argv, None);
        assert_eq!(
            r.proxy_jump.as_deref(),
            Some("good,-oProxyCommand=id"),
            "display keeps it"
        );

        let group = HostGroup {
            id: 1,
            name: "g".into(),
            sort_order: 0,
            default_identity_id: None,
            default_username: None,
            default_port: None,
            default_proxy_jump: Some("-oLocalCommand=id".into()),
            default_transport: None,
            default_forward_agent: None,
            parent_id: None,
            reserved: false,
        };
        assert_eq!(
            ResolvedConnection::for_group_chain(&[group]).jump_argv,
            None
        );

        let mut ok = NewHost::launcher("ok", "10.0.0.2");
        ok.proxy_jump = Some("bastion,ops@10.0.0.9:2222".into());
        let ok = store.create_host(&ok).unwrap();
        assert_eq!(
            ResolvedConnection::from_stored(&ok).jump_argv.as_deref(),
            Some("bastion,ops@10.0.0.9:2222")
        );
    }

    #[test]
    fn explicit_host_values_win_over_group() {
        // Oracle: hand-computed — every host-explicit value must survive, and
        // the group defaults must not leak into any field.
        let store = memory();
        let group = full_group(&store, "prod", None);
        let iid = identity(&store, "host-ident", "host-id-user");
        let mut nh = NewHost::launcher("web", "10.0.0.1");
        nh.group_id = Some(group.id);
        nh.identity_id = Some(iid);
        nh.username = Some("deploy".into());
        nh.port = Some(2222);
        nh.proxy_jump = Some("direct-bastion".into());
        nh.transport = Some(SessionTransport::Ssh);
        nh.forward_agent = Some(false);
        let host = store.create_host(&nh).unwrap();

        let resolved = store.resolve_connection(&host).unwrap();
        assert_eq!(
            resolved,
            ResolvedConnection {
                identity_id: Some(iid),
                identity: store.get_identity(iid).unwrap(),
                username: Some("deploy".into()),
                port: 2222,
                proxy_jump: Some("direct-bastion".into()),
                jump_argv: Some("direct-bastion".into()),
                jump_error: None,
                transport: SessionTransport::Ssh,
                forward_agent: false,
            }
        );
    }

    #[test]
    fn unset_host_fields_inherit_group() {
        // Oracle: hand-computed — with every host field unset, the resolved
        // values equal the group's defaults verbatim.
        let store = memory();
        let group = full_group(&store, "prod", None);
        let host = host_in(&store, "web", &group);

        let resolved = store.resolve_connection(&host).unwrap();
        assert_eq!(
            resolved,
            ResolvedConnection {
                identity_id: group.default_identity_id,
                identity: group
                    .default_identity_id
                    .and_then(|id| store.get_identity(id).unwrap()),
                username: Some("prod-login".into()),
                port: 2200,
                proxy_jump: Some("prod-bastion".into()),
                jump_argv: Some("prod-bastion".into()),
                jump_error: None,
                transport: SessionTransport::Mosh,
                forward_agent: true,
            }
        );
    }

    #[test]
    fn nested_child_overrides_parent_per_field() {
        // Oracle: hand-computed vector over three levels. Each level sets a
        // disjoint subset; the resolved value of every field must come from
        // the NEAREST level that sets it.
        let store = memory();
        let root = full_group(&store, "root", None);
        // Parent overrides only port and transport.
        let parent = store
            .create_group(&NewHostGroup {
                name: "parent".into(),
                default_port: Some(2201),
                default_transport: Some(SessionTransport::Ssh),
                parent_id: Some(root.id),
                ..Default::default()
            })
            .unwrap();
        // Child overrides only the username.
        let child = store
            .create_group(&NewHostGroup {
                name: "child".into(),
                default_username: Some("child-login".into()),
                parent_id: Some(parent.id),
                ..Default::default()
            })
            .unwrap();
        let host = host_in(&store, "web", &child);

        let resolved = store.resolve_connection(&host).unwrap();
        assert_eq!(
            resolved,
            ResolvedConnection {
                // nearest setter is the root (parent/child set no identity)
                identity_id: root.default_identity_id,
                identity: root
                    .default_identity_id
                    .and_then(|id| store.get_identity(id).unwrap()),
                // nearest setter is the child
                username: Some("child-login".into()),
                // nearest setter is the parent
                port: 2201,
                // nearest setter is the root
                proxy_jump: Some("root-bastion".into()),
                jump_argv: Some("root-bastion".into()),
                jump_error: None,
                // nearest setter is the parent
                transport: SessionTransport::Ssh,
                // nearest setter is the root
                forward_agent: true,
            }
        );
    }

    #[test]
    fn no_group_falls_back_to_global_defaults() {
        // Oracle: the documented global defaults, spelled out literally.
        let store = memory();
        let host = store
            .create_host(&NewHost::launcher("lonely", "10.9.9.9"))
            .unwrap();

        assert_eq!(
            store.resolve_connection(&host).unwrap(),
            ResolvedConnection::global_default()
        );
        assert_eq!(ResolvedConnection::global_default().port, 22);
        assert_eq!(
            ResolvedConnection::global_default().transport,
            SessionTransport::Ssh
        );
        assert!(!ResolvedConnection::global_default().forward_agent);
    }

    #[test]
    fn group_identity_resolves_for_auth_fallback() {
        // Oracle: the host sets no identity; the resolved identity must be
        // the group's default, loaded with its username for argv fallback.
        let store = memory();
        let group = full_group(&store, "prod", None);
        let host = host_in(&store, "web", &group);

        let resolved = store.resolve_connection(&host).unwrap();
        assert_eq!(resolved.identity_id, group.default_identity_id);
        let ident = resolved.identity.unwrap();
        assert_eq!(ident.username.as_deref(), Some("prod-user"));
    }

    #[test]
    fn ancestor_chain_is_nearest_first_and_resists_cycles() {
        // Oracle: exact id sequence for a three-level tree, then termination
        // (not a hang) once a parent cycle is introduced at the store level.
        let store = memory();
        let root = full_group(&store, "root", None);
        let parent = store
            .create_group(&NewHostGroup {
                name: "parent".into(),
                parent_id: Some(root.id),
                ..Default::default()
            })
            .unwrap();
        let child = store
            .create_group(&NewHostGroup {
                name: "child".into(),
                parent_id: Some(parent.id),
                ..Default::default()
            })
            .unwrap();

        let ids: Vec<i64> = store
            .group_ancestor_chain(Some(child.id))
            .unwrap()
            .into_iter()
            .map(|g| g.id)
            .collect();
        assert_eq!(ids, vec![child.id, parent.id, root.id]);

        // Introduce a cycle root -> child (the UI forbids this via
        // eligible_parents, but the store must not hang if one ever lands).
        store
            .update_group(
                root.id,
                &HostGroupUpdate {
                    parent_id: Some(Some(child.id)),
                    ..Default::default()
                },
            )
            .unwrap();
        let ids: Vec<i64> = store
            .group_ancestor_chain(Some(child.id))
            .unwrap()
            .into_iter()
            .map(|g| g.id)
            .collect();
        assert_eq!(ids, vec![child.id, parent.id, root.id]);
    }

    #[test]
    fn clearing_host_field_restores_inheritance() {
        // Oracle: end-to-end through HostUpdate — clearing the port (inner
        // None) must resolve to the group default again, not to 22 or the
        // previous explicit value.
        let store = memory();
        let group = full_group(&store, "prod", None);
        let host = host_in(&store, "web", &group);
        store
            .update_host(
                host.id,
                &HostUpdate {
                    port: Some(Some(2222)),
                    ..Default::default()
                },
            )
            .unwrap();
        let explicit = store.get_host(host.id).unwrap().unwrap();
        assert_eq!(store.resolve_connection(&explicit).unwrap().port, 2222);

        store
            .update_host(
                host.id,
                &HostUpdate {
                    port: Some(None),
                    ..Default::default()
                },
            )
            .unwrap();
        let cleared = store.get_host(host.id).unwrap().unwrap();
        assert_eq!(cleared.port, None);
        assert_eq!(store.resolve_connection(&cleared).unwrap().port, 2200);
    }

    /// A host joined to every group in `ids`, with any identity that
    /// `set_host_groups` materialized cleared again so the test exercises
    /// group inheritance (not the materialized host-explicit value).
    fn host_in_all(store: &LauncherStore, name: &str, ids: &[i64]) -> ManagedHost {
        let mut nh = NewHost::launcher(name, "10.0.0.1");
        nh.port = None;
        nh.forward_agent = None;
        nh.transport = None;
        let created = store.create_host(&nh).unwrap();
        store.set_host_groups(created.id, ids).unwrap();
        store
            .update_host(
                created.id,
                &HostUpdate {
                    identity_id: Some(None),
                    ..Default::default()
                },
            )
            .unwrap();
        store.get_host(created.id).unwrap().unwrap()
    }

    #[test]
    fn multi_group_conflict_resolves_to_deeper_group() {
        // Oracle: hand-computed — Tester sits directly in both the parent
        // (identity A) and its child (identity B). Both candidates are chain
        // distance 0, so the deeper group (child) wins and identity B
        // resolves. The parent sorts first, hence is primary: depth outranks
        // primary here.
        let store = memory();
        let parent_ident = identity(&store, "parent-ident", "parent-user");
        let child_ident = identity(&store, "child-ident", "child-user");
        let parent = store
            .create_group(&NewHostGroup {
                name: "aparent".into(),
                default_identity_id: Some(parent_ident),
                ..Default::default()
            })
            .unwrap();
        let child = store
            .create_group(&NewHostGroup {
                name: "zchild".into(),
                default_identity_id: Some(child_ident),
                parent_id: Some(parent.id),
                ..Default::default()
            })
            .unwrap();
        let host = host_in_all(&store, "Tester", &[parent.id, child.id]);
        assert_eq!(host.group_id, Some(parent.id));
        assert_eq!(host.identity_id, None);

        let resolved = store.resolve_connection(&host).unwrap();
        assert_eq!(resolved.identity_id, Some(child_ident));
        assert_eq!(resolved.identity, store.get_identity(child_ident).unwrap());
    }

    #[test]
    fn multi_group_distance_beats_depth() {
        // Oracle: hand-computed — the host sits directly in the root
        // (identity A at depth 0, distance 0) and in a grandchild whose
        // parent sets identity B (depth 1, distance 1). The nearer default
        // wins even though its group is shallower. The leaf sorts first,
        // hence is primary: distance outranks both depth and primary here.
        let store = memory();
        let root_ident = identity(&store, "root-ident", "root-user");
        let mid_ident = identity(&store, "mid-ident", "mid-user");
        let root = store
            .create_group(&NewHostGroup {
                name: "zroot".into(),
                default_identity_id: Some(root_ident),
                ..Default::default()
            })
            .unwrap();
        let mid = store
            .create_group(&NewHostGroup {
                name: "mmid".into(),
                default_identity_id: Some(mid_ident),
                parent_id: Some(root.id),
                ..Default::default()
            })
            .unwrap();
        let leaf = store
            .create_group(&NewHostGroup {
                name: "aleaf".into(),
                parent_id: Some(mid.id),
                ..Default::default()
            })
            .unwrap();
        let host = host_in_all(&store, "roamer", &[root.id, leaf.id]);
        assert_eq!(host.group_id, Some(leaf.id));
        assert_eq!(host.identity_id, None);

        let resolved = store.resolve_connection(&host).unwrap();
        assert_eq!(resolved.identity_id, Some(root_ident));
        assert_eq!(resolved.identity, store.get_identity(root_ident).unwrap());
    }

    #[test]
    fn join_flow_materializes_most_specific_identity() {
        // Oracle: end-to-end through the real join flow with NO manual
        // clearing — Tester joins parent (identity A, primary by name order)
        // and child (identity B). The join must store the most-specific
        // winner (child: both distance 0, child deeper), not the primary
        // default, and resolution must return B.
        let store = memory();
        let parent_ident = identity(&store, "parent-ident", "parent-user");
        let child_ident = identity(&store, "child-ident", "child-user");
        let parent = store
            .create_group(&NewHostGroup {
                name: "aparent".into(),
                default_identity_id: Some(parent_ident),
                ..Default::default()
            })
            .unwrap();
        let child = store
            .create_group(&NewHostGroup {
                name: "zchild".into(),
                default_identity_id: Some(child_ident),
                parent_id: Some(parent.id),
                ..Default::default()
            })
            .unwrap();
        let mut nh = NewHost::launcher("Tester", "10.0.0.1");
        nh.port = None;
        nh.forward_agent = None;
        nh.transport = None;
        let created = store.create_host(&nh).unwrap();
        store
            .set_host_groups(created.id, &[parent.id, child.id])
            .unwrap();
        let stored = store.get_host(created.id).unwrap().unwrap();
        assert_eq!(stored.group_id, Some(parent.id));
        assert_eq!(stored.identity_id, Some(child_ident));

        let resolved = store.resolve_connection(&stored).unwrap();
        assert_eq!(resolved.identity_id, Some(child_ident));
        assert_eq!(resolved.identity, store.get_identity(child_ident).unwrap());
    }

    #[test]
    fn a_user_with_an_at_sign_survives_expansion_and_bad_launcher_hops_are_refused() {
        let store = memory();
        assert_eq!(
            store.expand_jump("john@corp.com@10.0.0.1").unwrap(),
            "john@corp.com@10.0.0.1"
        );
        // A saved hop is rewritten to its address; a bad stored address is not
        // replaced by the hop's name (ssh would resolve that to another host).
        store
            .create_host(&NewHost::launcher("good", "10.0.0.9"))
            .unwrap();
        assert_eq!(store.expand_jump("good").unwrap(), "10.0.0.9");
        store
            .create_host(&NewHost::launcher("evil", "h;touch x"))
            .unwrap();
        assert!(store.expand_jump("evil").is_err());
    }

    #[test]
    fn a_host_that_jumps_through_itself_is_refused() {
        let store = memory();
        let mut nh = NewHost::launcher("bastion", "10.0.0.1");
        nh.proxy_jump = Some("bastion".into());
        let me = store.create_host(&nh).unwrap();
        let err = store.resolve_connection(&me).unwrap_err().to_string();
        assert!(err.contains("jumps through itself"), "{err}");
        // Through another host is fine.
        store
            .create_host(&NewHost::launcher("other", "10.0.0.2"))
            .unwrap();
        let mut nh = NewHost::launcher("db", "10.0.0.3");
        nh.proxy_jump = Some("other".into());
        let db = store.create_host(&nh).unwrap();
        assert!(store.resolve_connection(&db).is_ok());
    }
}
