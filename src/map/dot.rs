//! Graphviz export of the map (`sshub map --dot`): the same forest the map tab
//! draws. `you` points at every root hop; a hop points at the hops behind it;
//! hosts hang off their last hop by dotted edges. A hop shared by two chains is
//! two nodes, as on screen. Jump loops are red, a raw hop is dashed, a
//! ProxyCommand hop is one dashed `proxy cmd` box (its command is never shown).

use std::fmt::Write;

use super::{HopKey, Map, PROXY_CMD_LABEL};

/// DOT string literal.
fn q(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            // A DOT line break; a stray CR is noise.
            '\n' => out.push_str("\\n"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn to_dot(map: &Map) -> String {
    let mut o = String::new();
    o.push_str("digraph sshub {\n");
    o.push_str("  rankdir=LR;\n  node [shape=box, fontname=\"monospace\"];\n");
    o.push_str("  you [label=\"you\", shape=oval];\n");

    for (i, n) in map.nodes.iter().enumerate() {
        let (label, style) = match &n.key {
            HopKey::Saved(h) => (map.hosts[*h].name.clone(), ""),
            HopKey::Raw(r) => (r.label(), ", style=dashed"),
            HopKey::ProxyCmd(_) => (PROXY_CMD_LABEL.to_string(), ", style=dashed"),
            HopKey::Invalid(t) => (format!("invalid: {t}"), ", style=dashed, color=red"),
        };
        let _ = writeln!(
            o,
            "  n{i} [label={}{style}];",
            q(&format!("{label}\n{} hosts", n.sigma))
        );
        match n.parent {
            Some(p) => {
                let _ = writeln!(o, "  n{p} -> n{i};");
            }
            None => {
                let _ = writeln!(o, "  you -> n{i};");
            }
        }
        for (_, hosts) in &n.cargo {
            for &h in hosts {
                let _ = writeln!(
                    o,
                    "  h{h} [label={}, shape=plaintext];\n  n{i} -> h{h} [style=dotted, arrowhead=none];",
                    q(&map.hosts[h].name)
                );
            }
        }
    }

    for (ci, c) in map.cycles.iter().enumerate() {
        let _ = writeln!(
            o,
            "  subgraph cluster_loop{ci} {{ label=\"jump loop\"; color=red;"
        );
        for (k, &h) in c.path[..c.path.len() - 1].iter().enumerate() {
            let _ = writeln!(
                o,
                "    c{ci}_{k} [label={}, color=red];",
                q(&map.hosts[h].name)
            );
        }
        let n = c.path.len() - 1;
        for k in 0..n {
            let _ = writeln!(o, "    c{ci}_{k} -> c{ci}_{} [color=red];", (k + 1) % n);
        }
        o.push_str("  }\n");
        let _ = writeln!(o, "  you -> c{ci}_0 [color=red, style=dashed];");
        for &h in &c.hosts {
            if !c.path.contains(&h) {
                let _ = writeln!(
                    o,
                    "  h{h} [label={}, shape=plaintext];\n  c{ci}_0 -> h{h} [color=red, style=dotted, arrowhead=none];",
                    q(&map.hosts[h].name)
                );
            }
        }
    }

    if !map.direct.is_empty() {
        let _ = writeln!(
            o,
            "  direct [label={}, style=dashed];\n  you -> direct [style=dashed];",
            q(&format!("direct-only\n{} hosts, no jump", map.direct.len()))
        );
    }
    o.push_str("}\n");
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{JumpSource, MapHost};

    fn host(name: &str, jump: Option<&str>, ssh_config: bool, pc: bool) -> MapHost {
        MapHost {
            name: name.into(),
            address: format!("{name}.x"),
            port: 22,
            group: "g".into(),
            jump: jump.map(String::from),
            jump_source: jump.map(|_| JumpSource::Own),
            ssh_config,
            proxy_command: pc,
            ..Default::default()
        }
    }

    #[test]
    fn dot_has_edges_hosts_loops_and_never_the_proxy_command() {
        let map = Map::build(vec![
            host("bastion", None, false, false),
            host("db", Some("bastion"), false, false),
            host("quoted\"name", Some("ops@203.0.113.7:22"), false, false),
            host("iot", None, true, true),
            host("lone", None, false, false),
            host("a", Some("b"), true, false),
            host("b", Some("a"), true, false),
        ]);
        let dot = to_dot(&map);
        assert!(dot.starts_with("digraph sshub {"));
        assert!(dot.trim_end().ends_with('}'));
        assert!(dot.contains("you -> n0"), "{dot}");
        assert!(dot.contains("label=\"db\""), "{dot}");
        assert!(dot.contains("style=dotted"), "{dot}");
        assert!(dot.contains("cluster_loop0"), "{dot}");
        assert!(dot.contains("color=red"), "{dot}");
        assert!(dot.contains("proxy cmd"), "{dot}");
        assert!(dot.contains("ops@203.0.113.7"), "{dot}");
        assert!(dot.contains("quoted\\\"name"), "quotes are escaped: {dot}");
        assert!(dot.contains("direct-only"), "{dot}");
        // Balanced braces: every `{` closes.
        assert_eq!(dot.matches('{').count(), dot.matches('}').count());
    }

    /// Oracle: Graphviz itself must accept what we print, when installed.
    #[test]
    fn graphviz_parses_the_output() {
        let map = Map::build(vec![
            host("bastion", None, false, false),
            host("db", Some("bastion"), false, false),
            host("a", Some("b"), true, false),
            host("b", Some("a"), true, false),
        ]);
        let dot = to_dot(&map);
        let Ok(mut child) = std::process::Command::new("dot")
            .args(["-Tsvg", "-o", "/dev/null"])
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        else {
            crate::test_support::tool_missing("graphviz (dot)");
            return;
        };
        use std::io::Write as _;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(dot.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "dot rejected the output: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
