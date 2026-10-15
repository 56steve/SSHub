//! Network map model (issue #79): the jump-host forest behind the map tab.
//!
//! Pure data, no UI. A host's *chain* is the ordered list of hops `ssh` takes
//! to reach it. Edges are real jumps only: the host's own ProxyJump, a group
//! default it inherits, or an ssh_config `ProxyJump` (as reported by
//! `ssh -G`). Groups and tags never create an edge.
//!
//! Chain rule (matches what `ssh -J` does, checked against `ssh -vv`, and
//! `LauncherStore::expand_jump`): the first token of a host's jump list also
//! brings that hop's own chain; every later token is pinned by the `-J` list,
//! so the host's explicit list overrides that hop's own setting. That is why
//! one hop can sit under two parents. `ProxyJump none` is an empty chain.
//!
//! Hop tokens are untrusted text (imports write them): anything
//! `parse_adhoc_target` refuses, such as a leading `-`, is an `Invalid` hop,
//! never a connectable one. A `ProxyCommand` is one opaque hop with a fixed
//! label; its text is never stored or shown.

mod build;

pub use build::collect_hosts;

use std::collections::{BTreeMap, HashMap, HashSet};

/// Where one hop of a chain was configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JumpSource {
    /// The host's own ProxyJump field.
    Own,
    /// A group default; carries the group's display path.
    Inherited { group: String },
    /// `ProxyJump` from ssh_config (via `ssh -G`).
    SshConfig,
}

/// One host as the map sees it: effective values, inheritance already applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MapHost {
    pub name: String,
    pub address: String,
    pub user: Option<String>,
    pub port: u16,
    /// Group display path (`prod/db`), `_ungrouped` when none.
    pub group: String,
    pub tags: Vec<String>,
    /// Effective ProxyJump list (comma separated), `None` when direct.
    pub jump: Option<String>,
    pub jump_source: Option<JumpSource>,
    pub ssh_config: bool,
    /// ssh_config sets a `ProxyCommand`: an opaque hop, never parsed.
    pub proxy_command: bool,
}

/// A hop token that is not a saved host: `[user@]host[:port]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RawHop {
    pub user: Option<String>,
    /// Without brackets for an IPv6 literal.
    pub host: String,
    pub port: Option<u16>,
    pub ipv6: bool,
}

impl RawHop {
    /// `user@host:port` as `ssh -J` takes it (IPv6 hosts bracketed).
    pub fn label(&self) -> String {
        let mut s = String::new();
        if let Some(u) = &self.user {
            s.push_str(u);
            s.push('@');
        }
        match (self.ipv6, self.port) {
            (true, Some(p)) => s.push_str(&format!("[{}]:{p}", self.host)),
            (true, None) => s.push_str(&format!("[{}]", self.host)),
            (false, Some(p)) => s.push_str(&format!("{}:{p}", self.host)),
            (false, None) => s.push_str(&self.host),
        }
        s
    }
}

/// Parse one hop token (`[user@]host[:port]`, optionally `ssh://`-prefixed).
/// Shares the ad-hoc connect parser, so a token ssh could read as an option
/// (leading `-`), or with whitespace or control characters, is refused.
pub fn parse_raw_hop(token: &str) -> Option<RawHop> {
    let token = token.trim();
    let t = crate::app::adhoc::parse_adhoc_target(token.strip_prefix("ssh://").unwrap_or(token))?;
    Some(RawHop {
        user: t.user,
        host: t.host,
        port: t.port,
        ipv6: t.ipv6,
    })
}

/// `ProxyJump none` / `-J none`: no jump at all.
pub fn is_jump_none(jump: &str) -> bool {
    jump.trim().eq_ignore_ascii_case("none")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HopKind {
    /// Index into [`Map::hosts`].
    Saved(usize),
    Raw(RawHop),
    /// The opaque ProxyCommand of host `n`.
    ProxyCommand(usize),
    /// A token ssh could not safely take as a host.
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop {
    pub kind: HopKind,
    pub source: JumpSource,
}

impl Hop {
    pub fn key(&self) -> HopKey {
        match &self.kind {
            HopKind::Saved(s) => HopKey::Saved(*s),
            HopKind::Raw(r) => HopKey::Raw(r.clone()),
            HopKind::ProxyCommand(h) => HopKey::ProxyCmd(*h),
            HopKind::Invalid(t) => HopKey::Invalid(t.clone()),
        }
    }
}

/// How a host is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chain {
    /// Hops in order; empty = direct.
    Hops(Vec<Hop>),
    /// A jump loop, in order, the first host repeated at the end.
    Cycle(Vec<usize>),
}

/// Identity of a hop node, for the trie and blast radius.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HopKey {
    Saved(usize),
    Raw(RawHop),
    /// The opaque ProxyCommand of host `n`.
    ProxyCmd(usize),
    Invalid(String),
}

/// Fixed label of a ProxyCommand hop: the command text is never shown.
pub const PROXY_CMD_LABEL: &str = "proxy cmd";

/// A jump token as a saved host: the whole token is a host name, or a
/// `[user@]name[:port]` form of one (ssh then looks the name up in config).
fn lookup_saved(index: &HashMap<&str, usize>, token: &str) -> Option<usize> {
    if let Some(&h) = index.get(token) {
        return Some(h);
    }
    let raw = parse_raw_hop(token)?;
    if raw.ipv6 {
        return None;
    }
    index.get(raw.host.as_str()).copied()
}

fn expand(
    hosts: &[MapHost],
    index: &HashMap<&str, usize>,
    idx: usize,
    visiting: &mut Vec<usize>,
) -> Result<Vec<Hop>, Vec<usize>> {
    let host = &hosts[idx];
    if host.proxy_command {
        return Ok(vec![Hop {
            kind: HopKind::ProxyCommand(idx),
            source: JumpSource::SshConfig,
        }]);
    }
    let Some(jump) = host.jump.as_deref().filter(|j| !is_jump_none(j)) else {
        return Ok(Vec::new());
    };
    let source = host.jump_source.clone().unwrap_or(JumpSource::Own);
    let mut out = Vec::new();
    for (i, token) in jump
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .enumerate()
    {
        let Some(h) = lookup_saved(index, token) else {
            let kind = match parse_raw_hop(token) {
                Some(raw) => HopKind::Raw(raw),
                None => HopKind::Invalid(token.to_string()),
            };
            out.push(Hop {
                kind,
                source: source.clone(),
            });
            continue;
        };
        if let Some(pos) = visiting.iter().position(|&v| v == h) {
            let mut cycle = visiting[pos..].to_vec();
            cycle.push(h);
            return Err(cycle);
        }
        if i == 0 {
            visiting.push(h);
            let own = expand(hosts, index, h, visiting);
            visiting.pop();
            out.extend(own?);
        }
        out.push(Hop {
            kind: HopKind::Saved(h),
            source: source.clone(),
        });
    }
    Ok(out)
}

/// Resolve the chain of `hosts[idx]`. `index` maps a host name to its index.
/// The visited set is per resolution path (so a hop shared by two branches is
/// not a loop) and seeded with the target.
fn resolve_chain(hosts: &[MapHost], index: &HashMap<&str, usize>, idx: usize) -> Chain {
    match expand(hosts, index, idx, &mut vec![idx]) {
        Ok(hops) => Chain::Hops(hops),
        Err(cycle) => Chain::Cycle(cycle),
    }
}

/// One trie node: a distinct chain prefix.
#[derive(Debug, Clone)]
pub struct Node {
    pub key: HopKey,
    pub parent: Option<usize>,
    pub depth: usize,
    pub children: Vec<usize>,
    /// Hosts whose chain ends here, grouped by host group, names sorted.
    pub cargo: Vec<(String, Vec<usize>)>,
    /// Hosts behind this node at any depth.
    pub sigma: usize,
}

/// One line of the map, top to bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    /// A hop node (depth-first: a parent, then its children).
    Node(usize),
    /// A jump loop, by index in [`Map::cycles`].
    Cycle(usize),
    /// The hosts with no jump.
    Direct,
}

/// A jump loop and the hosts stuck behind it.
#[derive(Debug, Clone)]
pub struct CycleRow {
    pub path: Vec<usize>,
    pub hosts: Vec<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct Map {
    pub hosts: Vec<MapHost>,
    pub chains: Vec<Chain>,
    pub nodes: Vec<Node>,
    pub roots: Vec<usize>,
    /// Hosts with no jump that nothing jumps through.
    pub direct: Vec<usize>,
    pub cycles: Vec<CycleRow>,
    /// Final trie node of each host's chain (`None`: direct or cyclic).
    pub host_node: Vec<Option<usize>>,
}

impl Map {
    pub fn build(hosts: Vec<MapHost>) -> Map {
        let mut index: HashMap<&str, usize> = HashMap::new();
        for (i, h) in hosts.iter().enumerate() {
            index.entry(h.name.as_str()).or_insert(i);
        }
        let chains: Vec<Chain> = (0..hosts.len())
            .map(|i| resolve_chain(&hosts, &index, i))
            .collect();

        let mut order: Vec<usize> = (0..hosts.len()).collect();
        order.sort_by(|&a, &b| hosts[a].name.cmp(&hosts[b].name));

        let mut map = Map {
            host_node: vec![None; hosts.len()],
            ..Map::default()
        };
        let mut child_of: HashMap<(Option<usize>, HopKey), usize> = HashMap::new();
        let mut cargo: Vec<BTreeMap<String, Vec<usize>>> = Vec::new();
        let mut used: HashSet<usize> = HashSet::new();
        let mut loops: BTreeMap<Vec<usize>, CycleRow> = BTreeMap::new();

        for &i in &order {
            let keys: Vec<HopKey> = match &chains[i] {
                Chain::Hops(h) if h.is_empty() => continue,
                Chain::Hops(h) => h.iter().map(Hop::key).collect(),
                Chain::Cycle(path) => {
                    let mut key: Vec<usize> = path.clone();
                    key.sort_unstable();
                    key.dedup();
                    loops
                        .entry(key)
                        .or_insert_with(|| CycleRow {
                            path: path.clone(),
                            hosts: Vec::new(),
                        })
                        .hosts
                        .push(i);
                    continue;
                }
            };
            let mut parent = None;
            for key in keys {
                if let HopKey::Saved(s) = key {
                    used.insert(s);
                }
                let at = *child_of.entry((parent, key.clone())).or_insert_with(|| {
                    let n = map.nodes.len();
                    map.nodes.push(Node {
                        key,
                        parent,
                        depth: parent.map_or(0, |p| map.nodes[p].depth + 1),
                        children: Vec::new(),
                        cargo: Vec::new(),
                        sigma: 0,
                    });
                    cargo.push(BTreeMap::new());
                    match parent {
                        Some(p) => map.nodes[p].children.push(n),
                        None => map.roots.push(n),
                    }
                    n
                });
                parent = Some(at);
            }
            let last = parent.expect("non-empty chain");
            cargo[last]
                .entry(hosts[i].group.clone())
                .or_default()
                .push(i);
            map.host_node[i] = Some(last);
        }

        for (n, groups) in cargo.into_iter().enumerate() {
            map.nodes[n].cargo = groups.into_iter().collect();
            map.nodes[n].sigma = map.nodes[n].cargo.iter().map(|(_, h)| h.len()).sum();
        }
        // Parents are created before their children, so one reverse pass sums
        // every subtree.
        for n in (0..map.nodes.len()).rev() {
            if let Some(p) = map.nodes[n].parent {
                map.nodes[p].sigma += map.nodes[n].sigma;
            }
        }
        // Biggest subtree first, then by label, so the layout is stable.
        let label = |m: &Map, n: usize| match &m.nodes[n].key {
            HopKey::Saved(s) => hosts[*s].name.clone(),
            HopKey::Raw(r) => r.label(),
            HopKey::ProxyCmd(h) => format!("{PROXY_CMD_LABEL} {}", hosts[*h].name),
            HopKey::Invalid(t) => t.clone(),
        };
        let sorted = |list: Vec<usize>, m: &Map| {
            let mut list = list;
            list.sort_by(|&a, &b| {
                m.nodes[b]
                    .sigma
                    .cmp(&m.nodes[a].sigma)
                    .then_with(|| label(m, a).cmp(&label(m, b)))
            });
            list
        };
        let snapshot = map.clone();
        map.roots = sorted(std::mem::take(&mut map.roots), &snapshot);
        for n in 0..map.nodes.len() {
            map.nodes[n].children = sorted(std::mem::take(&mut map.nodes[n].children), &snapshot);
        }

        map.direct = order
            .iter()
            .copied()
            .filter(|i| matches!(&chains[*i], Chain::Hops(h) if h.is_empty()) && !used.contains(i))
            .collect();
        map.cycles = loops.into_values().collect();
        map.hosts = hosts;
        map.chains = chains;
        map
    }

    /// The map's rows in drawing order: every root subtree depth-first, then the
    /// jump loops, then the direct-only bucket.
    pub fn rows(&self) -> Vec<Row> {
        fn walk(map: &Map, n: usize, out: &mut Vec<Row>) {
            out.push(Row::Node(n));
            for &c in &map.nodes[n].children {
                walk(map, c, out);
            }
        }
        let mut rows = Vec::new();
        for &r in &self.roots {
            walk(self, r, &mut rows);
        }
        rows.extend((0..self.cycles.len()).map(Row::Cycle));
        if !self.direct.is_empty() {
            rows.push(Row::Direct);
        }
        rows
    }

    /// Hosts drawn as glyphs on `row`, left to right.
    pub fn row_hosts(&self, row: Row) -> Vec<usize> {
        match row {
            Row::Node(n) => self.nodes[n]
                .cargo
                .iter()
                .flat_map(|(_, h)| h.iter().copied())
                .collect(),
            Row::Cycle(i) => self.cycles[i].hosts.clone(),
            Row::Direct => self.direct.clone(),
        }
    }

    /// Trie nodes from the root down to `node`.
    pub fn node_path(&self, node: usize) -> Vec<usize> {
        let mut path = vec![node];
        while let Some(p) = self.nodes[*path.last().expect("non-empty")].parent {
            path.push(p);
        }
        path.reverse();
        path
    }

    /// Display name of a hop key.
    pub fn key_label(&self, key: &HopKey) -> String {
        match key {
            HopKey::Saved(s) => self.hosts[*s].name.clone(),
            HopKey::Raw(r) => r.label(),
            HopKey::ProxyCmd(_) => PROXY_CMD_LABEL.to_string(),
            HopKey::Invalid(t) => t.clone(),
        }
    }

    /// Hop keys `host` depends on, in order. A cyclic host depends on every
    /// host of its loop.
    pub fn chain_keys(&self, host: usize) -> Vec<HopKey> {
        match &self.chains[host] {
            Chain::Hops(h) => h.iter().map(Hop::key).collect(),
            Chain::Cycle(path) => path[..path.len() - 1]
                .iter()
                .map(|&s| HopKey::Saved(s))
                .collect(),
        }
    }

    /// Blast radius of a hop: every host whose resolved chain contains it at
    /// any depth, counted across all occurrences; hosts stuck in a loop count
    /// as dependents of the loop's hops. The hop's own host is not in its own
    /// radius.
    pub fn blast_radius(&self, key: &HopKey) -> Vec<usize> {
        let own = match key {
            HopKey::Saved(s) => Some(*s),
            _ => None,
        };
        (0..self.hosts.len())
            .filter(|&i| Some(i) != own)
            .filter(|&i| self.chain_keys(i).contains(key))
            .collect()
    }
}

#[cfg(test)]
mod tests;
