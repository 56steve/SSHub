//! Pure helpers behind the hop editor: the comma separated ProxyJump string as
//! a list, raw hop validation, routes already in use, and the hosts a hop
//! must not be (cycle guard).

use std::collections::HashSet;

use super::{is_jump_none, parse_raw_hop, HopKey, Map};

/// The hop tokens of a stored ProxyJump value. `none` (direct) and an empty
/// value are both an empty list.
pub fn split(value: &str) -> Vec<String> {
    if is_jump_none(value) {
        return Vec::new();
    }
    value
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

/// Back to the stored form: comma separated, no spaces.
pub fn join(hops: &[String]) -> String {
    hops.join(",")
}

/// A hop typed as a raw `[user@]host[:port]`: the canonical token to store, or
/// why it cannot be one. Reuses the ad-hoc connect parser, so an option-like
/// token (leading `-`) or one with whitespace is refused, and a comma would
/// split it into two hops.
pub fn validate_raw(input: &str) -> Result<String, &'static str> {
    let input = input.trim();
    if input.is_empty() {
        return Err("type [user@]host[:port]");
    }
    if input.contains(',') {
        return Err("one hop at a time, no commas");
    }
    match parse_raw_hop(input) {
        Some(raw) => Ok(raw.label()),
        None => Err("not a valid [user@]host[:port]"),
    }
}

/// Why a stored token cannot be saved as a hop, if it cannot.
pub fn token_issue(token: &str) -> Option<&'static str> {
    parse_raw_hop(token)
        .is_none()
        .then_some("not a valid [user@]host[:port]")
}

/// A route already in use: the hops from you to some hop, and how many hosts
/// sit behind its last hop at any depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub tokens: Vec<String>,
    pub hosts: usize,
}

/// Distinct chains in use, biggest first (`limit` of them). Chains through a
/// ProxyCommand or an invalid hop are not reusable and are skipped.
pub fn routes_in_use(map: &Map, limit: usize) -> Vec<Route> {
    let mut routes: Vec<Route> = Vec::new();
    for n in 0..map.nodes.len() {
        let mut tokens = Vec::new();
        let mut ok = true;
        for p in map.node_path(n) {
            match &map.nodes[p].key {
                HopKey::Saved(h) => tokens.push(map.hosts[*h].name.clone()),
                HopKey::Raw(r) => tokens.push(r.label()),
                HopKey::ProxyCmd(_) | HopKey::Invalid(_) => ok = false,
            }
        }
        if ok && !routes.iter().any(|r| r.tokens == tokens) {
            routes.push(Route {
                tokens,
                hosts: map.nodes[n].sigma,
            });
        }
    }
    routes.sort_by(|a, b| b.hosts.cmp(&a.hosts).then_with(|| a.tokens.cmp(&b.tokens)));
    routes.truncate(limit);
    routes
}

/// Names that must not become a hop of `edited`: the host itself and every
/// host that already depends on it (it would close a loop), including loops
/// that are already there.
pub fn forbidden_hops(map: &Map, edited: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    out.insert(edited.to_string());
    if let Some(h) = map.hosts.iter().position(|h| h.name == edited) {
        for d in map.blast_radius(&HopKey::Saved(h)) {
            out.insert(map.hosts[d].name.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{JumpSource, MapHost};

    fn host(name: &str, jump: Option<&str>, ssh_config: bool) -> MapHost {
        MapHost {
            name: name.into(),
            address: format!("{name}.x"),
            port: 22,
            group: "g".into(),
            jump: jump.map(String::from),
            jump_source: jump.map(|_| JumpSource::Own),
            ssh_config,
            ..Default::default()
        }
    }

    #[test]
    fn split_join_none_and_blanks() {
        assert_eq!(split("a, b ,,c"), ["a", "b", "c"]);
        assert_eq!(split("none"), Vec::<String>::new());
        assert_eq!(split("  "), Vec::<String>::new());
        assert_eq!(join(&split("a, b")), "a,b");
    }

    #[test]
    fn raw_validation_refuses_option_like_and_multi_hop_input() {
        assert_eq!(
            validate_raw("ops@10.0.0.1:2222").unwrap(),
            "ops@10.0.0.1:2222"
        );
        assert_eq!(validate_raw("[::1]:22").unwrap(), "[::1]:22");
        for bad in [
            "-oProxyCommand=id",
            "ops@-oProxyCommand=id",
            "a b",
            "a,b",
            "",
            "host:99999",
            "host:0",
        ] {
            assert!(validate_raw(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(token_issue("bastion-eu").is_none());
        assert!(token_issue("-J").is_some());
    }

    #[test]
    fn cycle_guard_names_the_host_and_everything_that_depends_on_it() {
        // c jumps via b, b via a (ssh_config hosts chain): a must not take b or c.
        let map = Map::build(vec![
            host("a", None, true),
            host("b", Some("a"), true),
            host("c", Some("b"), true),
            host("free", None, false),
        ]);
        let f = forbidden_hops(&map, "a");
        assert!(f.contains("a") && f.contains("b") && f.contains("c"));
        assert!(!f.contains("free"));
        // A brand new host (not in the map yet) only forbids itself.
        assert_eq!(forbidden_hops(&map, "new").len(), 1);
    }

    #[test]
    fn routes_in_use_are_distinct_and_biggest_first() {
        let map = Map::build(vec![
            host("bastion", None, false),
            host("jump", Some("bastion"), true),
            host("db-1", Some("jump"), false),
            host("db-2", Some("jump"), false),
            host("web", Some("bastion"), false),
        ]);
        let routes = routes_in_use(&map, 8);
        assert_eq!(routes[0].tokens, ["bastion"]);
        assert_eq!(routes[0].hosts, 4);
        assert_eq!(routes[1].tokens, ["bastion", "jump"]);
        assert!(routes.iter().all(|r| !r.tokens.is_empty()));
        let mut seen = HashSet::new();
        assert!(routes.iter().all(|r| seen.insert(r.tokens.clone())));
    }
}
