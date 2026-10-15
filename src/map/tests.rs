use super::*;

fn host(name: &str, group: &str, jump: Option<&str>) -> MapHost {
    MapHost {
        name: name.into(),
        address: format!("{name}.example"),
        user: Some("ops".into()),
        port: 22,
        group: group.into(),
        jump: jump.map(String::from),
        jump_source: jump.map(|_| JumpSource::Own),
        ..Default::default()
    }
}

fn ssh_config(name: &str, jump: Option<&str>, proxy_command: bool) -> MapHost {
    MapHost {
        ssh_config: true,
        proxy_command,
        jump_source: jump.map(|_| JumpSource::SshConfig),
        ..host(name, "_ungrouped", jump)
    }
}

fn idx(map: &Map, name: &str) -> usize {
    map.hosts.iter().position(|h| h.name == name).unwrap()
}

fn names(map: &Map, chain: &Chain) -> Vec<String> {
    match chain {
        Chain::Hops(h) => h
            .iter()
            .map(|hop| match &hop.kind {
                HopKind::Saved(s) => map.hosts[*s].name.clone(),
                HopKind::Raw(r) => format!("raw:{}", r.label()),
                HopKind::ProxyCommand(_) => PROXY_CMD_LABEL.into(),
                HopKind::Invalid(t) => format!("invalid:{t}"),
            })
            .collect(),
        Chain::Cycle(p) => p.iter().map(|&i| map.hosts[i].name.clone()).collect(),
    }
}

fn chain_of(map: &Map, name: &str) -> Vec<String> {
    names(map, &map.chains[idx(map, name)])
}

/// The fleet of the design mockups, scaled down.
fn fleet() -> Map {
    let mut hosts = vec![
        host("bastion-eu", "prod", None),
        ssh_config("jump-2", Some("bastion-eu"), false),
        host("web-1", "prod", Some("bastion-eu")),
        host("db-1", "prod/db", Some("jump-2")),
        host("db-2", "prod/db", Some("jump-2")),
        host("bastion-us", "us", None),
        host("us-1", "us", Some("bastion-us")),
        host("db-us-2", "us", Some("bastion-us,jump-2")),
        ssh_config("corp-gw", None, false),
        host("ci-1", "ci", Some("corp-gw")),
        host("legacy-1", "legacy", Some("ops@203.0.113.7:2222")),
        ssh_config("iot-1", None, true),
        ssh_config("loop-a", Some("loop-b"), false),
        ssh_config("loop-b", Some("loop-a"), false),
        host("standalone", "staging", None),
    ];
    hosts[3].jump_source = Some(JumpSource::Inherited {
        group: "prod/db".into(),
    });
    Map::build(hosts)
}

#[test]
fn first_hop_brings_its_own_chain_later_hops_are_pinned() {
    let m = fleet();
    // db-1 -> jump-2 -> (jump-2 jumps via bastion-eu)
    assert_eq!(chain_of(&m, "db-1"), ["bastion-eu", "jump-2"]);
    // db-us-2 = "bastion-us,jump-2": jump-2 is pinned behind bastion-us and
    // does NOT bring bastion-eu along.
    assert_eq!(chain_of(&m, "db-us-2"), ["bastion-us", "jump-2"]);
    assert_eq!(chain_of(&m, "bastion-eu"), Vec::<String>::new());
}

#[test]
fn launcher_hop_own_jump_is_never_followed() {
    // `-J` gets `user@addr` for a launcher hop; ssh cannot see its own jump.
    let m = Map::build(vec![
        host("bastion", "g", None),
        host("jump", "g", Some("bastion")),
        host("db", "g", Some("jump")),
    ]);
    assert_eq!(chain_of(&m, "db"), ["jump"]);
    assert_eq!(chain_of(&m, "jump"), ["bastion"]);
}

#[test]
fn source_is_recorded_per_hop() {
    let m = fleet();
    let Chain::Hops(h) = &m.chains[idx(&m, "db-1")] else {
        panic!()
    };
    // bastion-eu comes from jump-2's own ssh_config, jump-2 from db-1's (group).
    assert_eq!(h[0].source, JumpSource::SshConfig);
    assert_eq!(
        h[1].source,
        JumpSource::Inherited {
            group: "prod/db".into()
        }
    );
}

#[test]
fn shared_hop_is_drawn_once_per_chain() {
    let m = fleet();
    let jump2 = idx(&m, "jump-2");
    let nodes: Vec<&Node> = m
        .nodes
        .iter()
        .filter(|n| n.key == HopKey::Saved(jump2))
        .collect();
    assert_eq!(
        nodes.len(),
        2,
        "jump-2 sits under bastion-eu and bastion-us"
    );
    let parents: HashSet<String> = nodes
        .iter()
        .map(|n| m.key_label(&m.nodes[n.parent.unwrap()].key))
        .collect();
    assert_eq!(
        parents,
        HashSet::from(["bastion-eu".to_string(), "bastion-us".to_string()])
    );
}

#[test]
fn sigma_cargo_and_direct_bucket() {
    let m = fleet();
    let root = |name: &str| {
        let i = idx(&m, name);
        m.roots
            .iter()
            .copied()
            .find(|&r| m.nodes[r].key == HopKey::Saved(i))
            .unwrap()
    };
    // bastion-eu: web-1 and jump-2 directly, db-1 and db-2 behind jump-2.
    assert_eq!(m.nodes[root("bastion-eu")].sigma, 4);
    assert_eq!(m.nodes[root("bastion-us")].sigma, 2);
    // Direct-only: no jump and nothing jumps through them.
    let direct: Vec<&str> = m.direct.iter().map(|&i| m.hosts[i].name.as_str()).collect();
    assert_eq!(direct, ["standalone"]);
    // Roots are sorted by subtree size.
    assert_eq!(m.key_label(&m.nodes[m.roots[0]].key), "bastion-eu");
}

#[test]
fn blast_radius_counts_every_occurrence_of_the_hop() {
    let m = fleet();
    let eu = HopKey::Saved(idx(&m, "bastion-eu"));
    let mut got: Vec<&str> = m
        .blast_radius(&eu)
        .into_iter()
        .map(|i| m.hosts[i].name.as_str())
        .collect();
    got.sort();
    // db-us-2 is NOT behind bastion-eu: its jump-2 is pinned to bastion-us.
    assert_eq!(got, ["db-1", "db-2", "jump-2", "web-1"]);
    let j2 = HopKey::Saved(idx(&m, "jump-2"));
    let mut got: Vec<&str> = m
        .blast_radius(&j2)
        .into_iter()
        .map(|i| m.hosts[i].name.as_str())
        .collect();
    got.sort();
    assert_eq!(got, ["db-1", "db-2", "db-us-2"]);
}

#[test]
fn cycle_is_flagged_and_counts_in_the_loops_blast_radius() {
    let m = fleet();
    assert_eq!(chain_of(&m, "loop-a"), ["loop-a", "loop-b", "loop-a"]);
    assert_eq!(chain_of(&m, "loop-b"), ["loop-b", "loop-a", "loop-b"]);
    assert_eq!(m.cycles.len(), 1);
    assert_eq!(m.cycles[0].hosts.len(), 2);
    // Cyclic hosts have no trie node, but are dependents of the loop's hops.
    assert_eq!(m.host_node[idx(&m, "loop-a")], None);
    let a = HopKey::Saved(idx(&m, "loop-a"));
    assert_eq!(m.blast_radius(&a), vec![idx(&m, "loop-b")]);
}

#[test]
fn diamond_is_not_a_cycle_and_self_jump_is() {
    // Per-path visited set: x -> m1,m2 where both m1 and m2 jump via base.
    let m = Map::build(vec![
        host("base", "g", None),
        ssh_config("m1", Some("base"), false),
        ssh_config("m2", Some("base"), false),
        host("x", "g", Some("m1,m2")),
        host("selfie", "g", Some("selfie")),
    ]);
    assert_eq!(chain_of(&m, "x"), ["base", "m1", "m2"]);
    assert_eq!(chain_of(&m, "selfie"), ["selfie", "selfie"]);
}

#[test]
fn jump_none_is_an_empty_chain() {
    let m = Map::build(vec![
        host("none-host", "g", Some("none")),
        host("b", "g", Some("None")),
    ]);
    assert_eq!(chain_of(&m, "none-host"), Vec::<String>::new());
    assert_eq!(chain_of(&m, "b"), Vec::<String>::new());
    assert_eq!(m.direct.len(), 2);
}

#[test]
fn raw_and_unknown_hops_and_url_forms() {
    let m = fleet();
    assert_eq!(chain_of(&m, "legacy-1"), ["raw:ops@203.0.113.7:2222"]);
    let m = Map::build(vec![
        host("b", "g", None),
        host("t1", "g", Some("ops@b:2200")),
        host("t2", "g", Some("ssh://[::1]:22")),
    ]);
    // `user@name:port` still names the saved host `b`.
    assert_eq!(chain_of(&m, "t1"), ["b"]);
    assert_eq!(chain_of(&m, "t2"), ["raw:[::1]:22"]);
}

#[test]
fn option_like_hop_is_invalid_never_a_host() {
    for token in [
        "-oProxyCommand=id",
        "-J",
        "ops@-oProxyCommand=id",
        "a b",
        "",
    ] {
        if token.is_empty() {
            continue;
        }
        let m = Map::build(vec![host("t", "g", Some(token))]);
        let Chain::Hops(h) = &m.chains[0] else {
            panic!()
        };
        assert!(
            matches!(h[0].kind, HopKind::Invalid(_)),
            "{token:?} must not become a hop: {:?}",
            h[0].kind
        );
    }
    assert!(parse_raw_hop("-oProxyCommand=id").is_none());
    assert!(parse_raw_hop("ops@203.0.113.7:2222").is_some());
}

#[test]
fn proxy_command_is_one_opaque_hop_without_its_text() {
    let m = fleet();
    assert_eq!(chain_of(&m, "iot-1"), [PROXY_CMD_LABEL]);
    let node = m.host_node[idx(&m, "iot-1")].unwrap();
    assert_eq!(m.key_label(&m.nodes[node].key), PROXY_CMD_LABEL);
    // A host jumping through a ProxyCommand host sits behind that opaque hop.
    let m = Map::build(vec![
        ssh_config("edge", None, true),
        host("behind", "g", Some("edge")),
    ]);
    assert_eq!(chain_of(&m, "behind"), [PROXY_CMD_LABEL, "edge"]);
}

#[test]
fn display_width_of_map_glyphs_is_one_cell() {
    // Every glyph the map draws is one terminal cell wide (design section 5).
    for g in "◉●×○▶◇↻⇄·»✓✗–┈┄═╘├└┬─▸┆‹›▾▸⋯Σ".chars()
    {
        assert_eq!(
            unicode_width::UnicodeWidthChar::width(g),
            Some(1),
            "{g:?} is not one cell"
        );
    }
}

/// Oracle: real `ssh -v` shows the path ssh takes. With `Host a / ProxyJump z`
/// in ssh_config, `ssh -J a,b t` goes z -> a -> b -> t: the first hop brings
/// its own chain, `b` is pinned by the `-J` list.
#[test]
fn chain_rule_matches_what_real_ssh_does() {
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
        "Host a\n  HostName a.invalid\n  ProxyJump z\nHost b\n  HostName b.invalid\n  ProxyJump y\n\
         Host z\n  HostName z.invalid\nHost y\n  HostName y.invalid\nHost t\n  HostName t.invalid\n",
    )
    .unwrap();
    let out = std::process::Command::new("ssh")
        .args(["-F"])
        .arg(&cfg)
        .args([
            "-v",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=2",
            "-J",
            "a,b",
            "t",
            "true",
        ])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stderr);
    let proxies: Vec<&str> = log
        .lines()
        .filter_map(|l| l.split("Executing proxy command: exec ").nth(1))
        .collect();
    assert!(proxies.len() >= 3, "ssh log: {log}");
    // Each proxy line is `ssh ... -W '[next]:22' via`; the innermost one ends
    // in the first host ssh dials. Real path = that host, then every `-W`
    // target except the final one, innermost first.
    let target = |l: &str| {
        let w = l.split("-W '[").nth(1).unwrap();
        w.split("]:").next().unwrap().to_string()
    };
    let via = |l: &str| l.split_whitespace().last().unwrap().to_string();
    let mut real: Vec<String> = vec![via(proxies.last().unwrap())];
    real.extend(proxies.iter().rev().skip(1).map(|l| via(l)));
    let mut targets: Vec<String> = proxies.iter().map(|l| target(l)).collect();
    targets.reverse();
    assert_eq!(targets.first().map(String::as_str), Some("a.invalid"));

    // Our model of the same config: target `t` jumps via `a,b`.
    let m = Map::build(vec![
        ssh_config("a", Some("z"), false),
        ssh_config("b", Some("y"), false),
        ssh_config("z", None, false),
        ssh_config("y", None, false),
        host("t", "g", Some("a,b")),
    ]);
    let ours: Vec<String> = chain_of(&m, "t");
    // ssh dialled z first, then a, then b (b's own `y` never appears).
    assert_eq!(ours, ["z", "a", "b"]);
    assert_eq!(real, ["z", "a", "b"], "ssh log: {log}");
    assert!(
        !log.contains("y.invalid"),
        "later hop must stay pinned: {log}"
    );
}
