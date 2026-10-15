//! The map's own filter grammar (never the hosts-list search): terms split on
//! spaces, all must hold.
//!
//! - `group:x`  host group path contains `x` (case-insensitive)
//! - `tag:x`    one of the host's tags contains `x`
//! - `via:x`    some hop on the host's path has a name containing `x`
//! - `!x`       host name does not contain `x`
//! - anything else: fuzzy match on the host name (nucleo, smart case)

use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};

use super::{HopKey, Map};

enum Term {
    Group(String),
    Tag(String),
    Via(String),
    Not(String),
    Name(Pattern),
}

pub struct MapFilter {
    terms: Vec<Term>,
}

impl MapFilter {
    pub fn parse(query: &str) -> Self {
        let terms = query
            .split_whitespace()
            .filter_map(|t| {
                let lower = |s: &str| s.to_lowercase();
                if let Some(v) = t.strip_prefix("group:") {
                    (!v.is_empty()).then(|| Term::Group(lower(v)))
                } else if let Some(v) = t.strip_prefix("tag:") {
                    (!v.is_empty()).then(|| Term::Tag(lower(v)))
                } else if let Some(v) = t.strip_prefix("via:") {
                    (!v.is_empty()).then(|| Term::Via(lower(v)))
                } else if let Some(v) = t.strip_prefix('!') {
                    (!v.is_empty()).then(|| Term::Not(lower(v)))
                } else {
                    Some(Term::Name(Pattern::parse(
                        t,
                        CaseMatching::Smart,
                        Normalization::Smart,
                    )))
                }
            })
            .collect();
        Self { terms }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Hosts of `map` that satisfy every term.
    pub fn matching(&self, map: &Map) -> Vec<usize> {
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf = Vec::new();
        (0..map.hosts.len())
            .filter(|&h| {
                self.terms
                    .iter()
                    .all(|t| term_holds(t, map, h, &mut matcher, &mut buf))
            })
            .collect()
    }
}

fn term_holds(t: &Term, map: &Map, h: usize, matcher: &mut Matcher, buf: &mut Vec<char>) -> bool {
    let host = &map.hosts[h];
    match t {
        Term::Group(g) => host.group.to_lowercase().contains(g),
        Term::Tag(g) => host.tags.iter().any(|x| x.to_lowercase().contains(g)),
        Term::Via(v) => map.chain_keys(h).iter().any(|k| match k {
            HopKey::ProxyCmd(_) => false,
            k => map.key_label(k).to_lowercase().contains(v),
        }),
        Term::Not(n) => !host.name.to_lowercase().contains(n),
        Term::Name(p) => {
            buf.clear();
            p.score(Utf32Str::new(&host.name, buf), matcher).is_some()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{JumpSource, MapHost};

    fn host(name: &str, group: &str, tags: &[&str], jump: Option<&str>) -> MapHost {
        MapHost {
            name: name.into(),
            address: format!("{name}.x"),
            port: 22,
            group: group.into(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            jump: jump.map(String::from),
            jump_source: jump.map(|_| JumpSource::Own),
            ..Default::default()
        }
    }

    fn names(map: &Map, q: &str) -> Vec<String> {
        MapFilter::parse(q)
            .matching(map)
            .into_iter()
            .map(|h| map.hosts[h].name.clone())
            .collect()
    }

    #[test]
    fn grammar_terms_combine_with_and() {
        let map = Map::build(vec![
            host("bastion-eu", "prod", &["bastion"], None),
            host("db-1", "prod/db", &["db", "pci"], Some("bastion-eu")),
            host("db-2", "prod/db", &["db"], Some("bastion-eu")),
            host("web-1", "us", &[], None),
        ]);
        assert_eq!(names(&map, "group:prod/db"), ["db-1", "db-2"]);
        assert_eq!(names(&map, "tag:pci"), ["db-1"]);
        assert_eq!(names(&map, "tag:db !db-2"), ["db-1"]);
        assert_eq!(names(&map, "via:bastion"), ["db-1", "db-2"]);
        assert_eq!(names(&map, "via:nothing"), Vec::<String>::new());
        assert_eq!(names(&map, "web"), ["web-1"]);
        assert_eq!(names(&map, "group:PROD tag:DB"), ["db-1", "db-2"]);
        // A bare prefix with no value is ignored, not an empty match-all/none.
        assert!(MapFilter::parse("tag:").is_empty());
    }
}
