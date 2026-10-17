//! The map against `docs/network-map/mockups/D*.txt` (issue #79).
//!
//! `fleet()` is the fictional fleet of the mockups (bastion-eu, jump-2,
//! bastion-us, corp-gw, a raw hop, a proxy cmd host, a loop, 188 direct hosts).
//! `mockup_key_rows_are_on_the_screen` renders every state and asserts that the
//! key rows of each mockup (taken from the `.txt` file, whitespace collapsed)
//! are on the real screen too; a needle that is not in the mockup file fails as
//! well, so the list cannot drift from the files. With `MAP_DUMP_DIR` set the
//! test also writes each state as a grid, to `diff` against the `.txt` files.

use std::collections::HashMap;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use sshub::app::{App, AppDeps};
use sshub::config::AppConfig;
use sshub::metadata::MetadataDb;
use sshub::ssh::{HostResolver, SshHost};
use sshub::store::{HostUpdate, LauncherStore, NewHost, NewHostGroup, NewTunnel, TunnelType};

/// ssh_config answers by name: the hosts that carry a jump or a ProxyCommand.
#[derive(Clone)]
struct FleetResolver {
    hosts: HashMap<String, SshHost>,
    order: Vec<String>,
}

impl HostResolver for FleetResolver {
    fn list_hosts(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.order.clone())
    }

    fn resolve_host(&self, name: &str) -> anyhow::Result<SshHost> {
        self.hosts
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown {name}"))
    }
}

fn config_host(name: &str, jump: Option<&str>, proxy_command: bool) -> SshHost {
    let mut h = SshHost::new(name);
    h.hostname = Some(format!("{name}.example"));
    h.port = Some(22);
    h.user = Some("ops".into());
    h.proxy_jump = jump.map(String::from);
    h.has_proxy_command = proxy_command;
    h
}

pub struct Fleet {
    pub app: App,
}

/// `direct` hosts with no jump, spread over the mockups' groups; `with_chains`
/// adds the jump hosts of D01.
pub fn fleet(direct: usize, with_chains: bool) -> Fleet {
    let store = Arc::new(LauncherStore::open_in_memory().unwrap());
    let group = |name: &str, parent: Option<i64>| {
        store
            .create_group(&NewHostGroup {
                name: name.into(),
                parent_id: parent,
                ..Default::default()
            })
            .unwrap()
            .id
    };
    let (mut config, mut order) = (HashMap::new(), Vec::new());
    let mut add_config = |name: &str, jump: Option<&str>, pc: bool| {
        config.insert(name.to_string(), config_host(name, jump, pc));
        order.push(name.to_string());
    };
    if with_chains {
        add_config("corp-gw", None, false);
        add_config("jump-2", Some("bastion-eu"), false);
        add_config("iot-1", None, true);
        add_config("loop-a", Some("loop-b"), false);
        add_config("loop-b", Some("loop-a"), false);
    }
    let resolver = FleetResolver {
        hosts: config,
        order,
    };
    let metadata = Arc::new(MetadataDb::default());
    let metadata_for_import = Arc::clone(&metadata);

    let launcher = |name: &str, g: Option<i64>, jump: Option<&str>, tags: &[&str]| {
        let mut h = NewHost::launcher(name, format!("{name}.example"));
        h.group_id = g;
        h.proxy_jump = jump.map(String::from);
        h.username = Some("ops".into());
        h.tags = tags.iter().map(|t| t.to_string()).collect();
        store.create_host(&h).unwrap();
    };
    if with_chains {
        let prod = group("prod", None);
        let db = group("db", Some(prod));
        let us = group("us", None);
        let ci = group("ci", None);
        let legacy = group("legacy", None);
        launcher("bastion-eu", Some(prod), None, &["bastion"]);
        launcher("bastion-us", Some(us), None, &[]);
        for i in 1..=8 {
            launcher(&format!("web-{i}"), Some(prod), Some("bastion-eu"), &[]);
        }
        for i in 1..=4 {
            launcher(&format!("db-{i}"), Some(db), Some("jump-2"), &["db", "pci"]);
        }
        for i in 1..=3 {
            launcher(&format!("us-{i}"), Some(us), Some("bastion-us"), &[]);
        }
        launcher("db-us-2", Some(us), Some("bastion-us,jump-2"), &["db"]);
        for i in 1..=3 {
            launcher(&format!("ci-{i}"), Some(ci), Some("corp-gw"), &[]);
        }
        launcher("legacy-1", Some(legacy), Some("203.0.113.7:2222"), &[]);
    }
    let names = [
        ("staging", 41),
        ("lab", 58),
        ("home", 9),
        ("k8s-nodes", 46),
        ("customers", 11),
    ];
    let mut left = direct;
    for (g, n) in names {
        let gid = group(g, None);
        for i in 0..n.min(left) {
            launcher(&format!("{g}-{i}"), Some(gid), None, &[]);
        }
        left = left.saturating_sub(n);
    }
    for i in 0..left {
        launcher(&format!("misc-{i}"), None, None, &[]);
    }

    let import_resolver = resolver.clone();
    let mut app = App::new_with_deps(
        AppConfig::default(),
        AppDeps {
            resolver: Box::new(resolver),
            metadata,
            store: Arc::clone(&store),
            password_store: Box::new(sshub::credentials::NoopPasswordStore),
        },
    );
    if with_chains {
        // Import the ssh_config hosts, then put them in their groups.
        sshub::ssh::import_ssh_config(&import_resolver, &store, &*metadata_for_import).unwrap();
        let lab = store
            .list_groups()
            .unwrap()
            .into_iter()
            .find(|g| g.name == "lab")
            .unwrap()
            .id;
        let ci = store
            .list_groups()
            .unwrap()
            .into_iter()
            .find(|g| g.name == "ci")
            .unwrap()
            .id;
        for (n, g) in [("loop-a", lab), ("loop-b", lab), ("corp-gw", ci)] {
            let h = store.get_host_by_name(n).unwrap().unwrap();
            store
                .update_host(
                    h.id,
                    &HostUpdate {
                        group_id: Some(Some(g)),
                        ..Default::default()
                    },
                )
                .unwrap();
        }
        for (host, ty, lp, rh, rp) in [
            ("bastion-eu", TunnelType::Local, 5432, "db-1", 5432),
            ("jump-2", TunnelType::Dynamic, 1080, "socks", 0),
            ("corp-gw", TunnelType::Remote, 9000, "localhost", 9000),
            ("db-1", TunnelType::Local, 6379, "cache-1", 6379),
            ("web-8", TunnelType::Local, 8080, "localhost", 80),
        ] {
            let h = store.get_host_by_name(host).unwrap().unwrap();
            store
                .create_tunnel(&NewTunnel {
                    host_id: Some(h.id),
                    tunnel_type: ty,
                    local_port: lp,
                    remote_host: rh.into(),
                    remote_port: rp,
                    label: None,
                    auto_connect: false,
                })
                .unwrap();
        }
    }
    app.reload_hosts().unwrap();
    app.reload_tunnels().unwrap();
    // Ping readings: the hops and most hosts answer, a few are slow or silent.
    for (name, ms) in [
        ("bastion-eu", 12),
        ("jump-2", 31),
        ("bastion-us", 88),
        ("corp-gw", 4),
    ] {
        app.ping_data.insert(name.into(), vec![ms]);
    }
    let names: Vec<String> = app.hosts.iter().map(|h| h.name().to_string()).collect();
    for (i, n) in names.iter().enumerate() {
        if app.ping_data.contains_key(n) {
            continue;
        }
        match i % 23 {
            3 => {} // never probed: unknown
            7 => {
                app.ping_data
                    .insert(n.clone(), vec![sshub::ping::PING_UNREACHABLE]);
            }
            11 => {
                app.ping_data.insert(n.clone(), vec![180]);
            }
            _ => {
                app.ping_data.insert(n.clone(), vec![20]);
            }
        }
    }
    Fleet { app }
}

/// 212 jump hosts in four groups plus `edge-gw` and `vault-core`, and 1,840
/// hosts in all, as in D08.
pub fn big_fleet() -> Fleet {
    let store = Arc::new(LauncherStore::open_in_memory().unwrap());
    let group = |name: &str| {
        store
            .create_group(&NewHostGroup {
                name: name.into(),
                ..Default::default()
            })
            .unwrap()
            .id
    };
    let add = |name: &str, g: Option<i64>, jump: Option<&str>| {
        let mut h = NewHost::launcher(name, format!("{name}.example"));
        h.group_id = g;
        h.proxy_jump = jump.map(String::from);
        store.create_host(&h).unwrap();
    };
    let clusters = [
        ("eu-bastions", "bastion-eu", 64),
        ("us-bastions", "bastion-us", 48),
        ("k8s-jumpboxes", "jump-k8s", 71),
        ("customer-gws", "gw-cust", 27),
    ];
    let mut jumps = 0;
    let mut names = Vec::new();
    for (g, prefix, n) in clusters {
        let gid = group(g);
        for i in 0..n {
            let name = format!("{prefix}-{i:02}");
            add(&name, Some(gid), None);
            names.push((name, g));
            jumps += 1;
        }
    }
    add("edge-gw", None, None);
    add("vault-core", None, Some("edge-gw"));
    jumps += 2;
    let fleet_group = group("fleet");
    let mut k = 0;
    while jumps + k < 1840 {
        let (via, _) = &names[k % names.len()];
        add(&format!("srv-{k}"), Some(fleet_group), Some(via));
        k += 1;
    }
    let mut app = App::new_with_deps(
        AppConfig::default(),
        AppDeps {
            resolver: Box::new(FleetResolver {
                hosts: HashMap::new(),
                order: Vec::new(),
            }),
            metadata: Arc::new(MetadataDb::default()),
            store,
            password_store: Box::new(sshub::credentials::NoopPasswordStore),
        },
    );
    app.reload_hosts().unwrap();
    for h in ["bastion-eu-00", "bastion-eu-01"] {
        app.ping_data.insert(h.into(), vec![12]);
    }
    Fleet { app }
}

/// `db-9`: a host in group `db` with no jump of its own.
fn add_db9(app: &mut App) {
    let db = app
        .store()
        .list_groups()
        .unwrap()
        .into_iter()
        .find(|g| g.name == "db")
        .unwrap();
    let mut h = NewHost::launcher("db-9", "db-9.example");
    h.group_id = Some(db.id);
    h.username = Some("postgres".into());
    app.store().create_host(&h).unwrap();
    app.reload_hosts().unwrap();
}

fn set_group_jump(app: &App, group: &str, jump: Option<&str>) {
    let g = app
        .store()
        .list_groups()
        .unwrap()
        .into_iter()
        .find(|g| g.name == group)
        .unwrap();
    app.store()
        .update_group(
            g.id,
            &sshub::store::HostGroupUpdate {
                default_proxy_jump: Some(jump.map(String::from)),
                ..Default::default()
            },
        )
        .unwrap();
}

fn open_form(app: &mut App, name: &str) {
    let idx = app.hosts.iter().position(|h| h.name() == name).unwrap();
    assert!(app.reveal_host(idx));
    press(app, 'e');
    app.host_form.as_mut().unwrap().field = sshub::app::HostFormField::ProxyJump;
}

/// Point host `name` at `127.0.0.1:port` so a test connect reaches a socket of
/// ours instead of a name that cannot resolve.
fn point_host_at(app: &mut App, name: &str, port: u16) {
    let h = app.store().get_host_by_name(name).unwrap().unwrap();
    app.store()
        .update_host(
            h.id,
            &HostUpdate {
                address: Some("127.0.0.1".into()),
                port: Some(Some(port)),
                ..Default::default()
            },
        )
        .unwrap();
    app.reload_hosts().unwrap();
}

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::empty())
}

pub fn press(app: &mut App, c: char) {
    let mods = if c.is_uppercase() {
        KeyModifiers::SHIFT
    } else {
        KeyModifiers::empty()
    };
    app.handle_key(KeyEvent::new(KeyCode::Char(c), mods))
        .unwrap();
}

pub fn type_str(app: &mut App, s: &str) {
    for c in s.chars() {
        press(app, c);
    }
}

pub fn grid(app: &App, w: u16, h: u16) -> Vec<String> {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| sshub::tui::render(f, app)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn to_map(app: &mut App) {
    app.terminal_area = ratatui::layout::Rect::new(0, 0, 160, 45);
    press(app, '6');
}

/// Move the cursor row `n` rows down.
fn down(app: &mut App, n: usize) {
    for _ in 0..n {
        app.handle_key(key(KeyCode::Down)).unwrap();
    }
}

fn dump(name: &str, lines: &[String]) {
    let Ok(dir) = std::env::var("MAP_DUMP_DIR") else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(format!("{dir}/{name}.txt"), lines.join("\n") + "\n").unwrap();
}

/// Rows with runs of blanks collapsed, so a column shifted by the fixture does
/// not matter, only the words and glyphs and their order.
fn collapsed(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

/// Every needle is a row fragment of `docs/network-map/mockups/<mockup>.txt`
/// and of the screen.
fn check(name: &str, mockup: &str, screen: &[String], needles: &[&str]) {
    dump(name, screen);
    let path = format!(
        "{}/docs/network-map/mockups/{mockup}.txt",
        env!("CARGO_MANIFEST_DIR")
    );
    let file: Vec<String> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    let (file, screen_rows) = (collapsed(&file), collapsed(screen));
    for needle in needles {
        assert!(
            file.iter().any(|l| l.contains(needle)),
            "{mockup}: {needle:?} is not in the mockup file"
        );
        assert!(
            screen_rows.iter().any(|l| l.contains(needle)),
            "{name}: {needle:?} is not on the screen:\n{}",
            screen.join("\n")
        );
    }
}

const FOOTER_D01: &str = "↑↓ row ←→ along the path ↵ connect / filter B blast o tunnel lens u use as jump t test ␣ fold e edit y copy cmd ? help q quit";
/// The first tree row of the fleet: the root hop and its edge.
const TREE_ROW: &str = "◉ you──┬─▸● bastion-eu";
const LEGEND_EDGE: &str = "─▸ jump edge ┈┈ \"hosts behind\" leader Σ all hosts behind, any depth";

#[test]
fn mockup_key_rows_are_on_the_screen() {
    // D12 and D13 run real ssh: under an empty HOME and ssh config, or not at all.
    if !super::support::have_ssh() {
        return;
    }
    let size = (160, 45);
    let render = |app: &App| grid(app, size.0, size.1);
    let tree = "─ map ─ hops as a tree · hosts as glyphs behind their last hop";

    // D01 default, D02 selected glyph, D03 blast, D04 filter, D05 tunnel lens.
    let mut f = fleet(188, true);
    to_map(&mut f.app);
    check("D01Default", "D01Default", &render(&f.app), &[TREE_ROW, tree, "─ path ribbon ─ bastion-eu", FOOTER_D01, "→ walks right along the row: hop → its glyphs, one host per step. ↵ connects to bastion-eu.", "─ legend ─", "─ node ─ bastion-eu", "Shift+Z expand into one glyph strip per group (a health board)", LEGEND_EDGE]);
    down(&mut f.app, 1);
    f.app.handle_key(key(KeyCode::Right)).unwrap();
    f.app.handle_key(key(KeyCode::Right)).unwrap();
    check(
        "D02SelectedDetail",
        "D02SelectedDetail",
        &render(&f.app),
        &[
            TREE_ROW,
            tree,
            FOOTER_D01,
            LEGEND_EDGE,
            "├─▸● bastion-us 88ms",
            "Shift+Z expand into one glyph strip per group (a health board)",
        ],
    );

    let mut f = fleet(188, true);
    to_map(&mut f.app);
    press(&mut f.app, 'B');
    check(
        "D03BlastRadius",
        "D03BlastRadius",
        &render(&f.app),
        &[
            TREE_ROW,
            "─ path ribbon ─ bastion-eu",
            "B blast off Esc clear n/N next dependent b broadcast to these y copy list ? help",
            "─ blast radius ─ bastion-eu",
            LEGEND_EDGE,
        ],
    );

    let mut f = fleet(188, true);
    to_map(&mut f.app);
    press(&mut f.app, '/');
    type_str(&mut f.app, "tag:db");
    f.app.handle_key(key(KeyCode::Enter)).unwrap();
    check(
        "D04Filter",
        "D04Filter",
        &render(&f.app),
        &[
            TREE_ROW,
            "─ map ─ filter · tag:db",
            "┆raw┆ ‹proxy cmd› ┄┄ ↻ ══ cycle ◇ reused hop",
        ],
    );

    let mut f = fleet(188, true);
    to_map(&mut f.app);
    down(&mut f.app, 1);
    f.app.handle_key(key(KeyCode::Right)).unwrap();
    press(&mut f.app, 'o');
    check("D05TunnelLens", "D05TunnelLens", &render(&f.app), &[TREE_ROW, "─ map ─ tunnel lens · ⇄ on hops and hosts that carry a tunnel", "╘══↻ loop-a ═════════════════▸↻ loop-b ══▸ loop-a ┈┈┈┈┈┈ lab ·· cycle · not connectable 2", "─ tunnels ─ 5", LEGEND_EDGE]);

    // D06: the cursor on the loop row.
    let mut f = fleet(188, true);
    to_map(&mut f.app);
    for _ in 0..40 {
        let on_cycle = f
            .app
            .map_view
            .as_ref()
            .and_then(|v| v.selection())
            .is_some_and(|s| matches!(s, sshub::app::network_map::MapSel::Cycle(_)));
        if on_cycle {
            break;
        }
        down(&mut f.app, 1);
    }
    check("D06EdgeCases", "D06EdgeCases", &render(&f.app), &[TREE_ROW, tree, "─ path ribbon ─ loop-a · cycle", "Not connectable. Clear ProxyJump on loop-a or loop-b, then t test. ↵ opens the hop editor."]);

    // D07 empty, D14 direct-only expanded (Shift+Z).
    let mut f = fleet(214, false);
    to_map(&mut f.app);
    check(
        "D07Empty",
        "D07Empty",
        &render(&f.app),
        &[
            tree,
            "All 214 hosts are reached directly, so there is no hop to hang them behind.",
            "A row appears when a host, its group, or ~/.ssh/config sets a ProxyJump.",
            "u on a host: route other hosts through it (fastest first edge)",
            "Shift+Z show the 214 direct hosts as a per-group health board",
            "e edit host G groups u use as jump Shift+Z show direct hosts 1 hosts ? help q quit",
        ],
    );
    press(&mut f.app, 'Z');
    check(
        "D14DirectOnlyExpanded",
        "D14DirectOnlyExpanded",
        &render(&f.app),
        &["Shift+Z collapse direct ␣ fold ↵ connect / filter ? help"],
    );

    // D09 at 100x30.
    let mut f = fleet(188, true);
    f.app.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
    press(&mut f.app, '6');
    down(&mut f.app, 1);
    f.app.handle_key(key(KeyCode::Right)).unwrap();
    f.app.handle_key(key(KeyCode::Right)).unwrap();
    check(
        "D09Narrow100x30",
        "D09Narrow100x30",
        &grid(&f.app, 100, 30),
        &[
            "you hop 1 hop 2 hosts behind this hop · one glyph each Σ",
            "↑↓ row ←→ path ↵ connect / filter B blast o lens ? help",
            "├─▸● bastion-us ─────┬┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈ us ●●● 4",
        ],
    );

    // D08: 212 jump hosts, 1,840 hosts, clustered.
    let mut f = big_fleet();
    to_map(&mut f.app);
    check("D08ManyJumpHosts", "D08ManyJumpHosts", &render(&f.app), &["you hop 1 · clusters hop 2 hosts behind · one glyph each Σ", "␣ open/close cluster → into cluster Shift+Z fold all / reveal B blast ↵ connect ? help"]);

    // D04 while the filter line is still open (the mockup's moment).
    let mut f = fleet(188, true);
    to_map(&mut f.app);
    press(&mut f.app, '/');
    type_str(&mut f.app, "tag:db");
    check(
        "D04FilterTyping",
        "D04Filter",
        &render(&f.app),
        &[
            "/ tag:db▏ 5 hosts match · rows without a match are hidden, hops above kept",
            "type filter Tab complete ↵ keep Esc clear # tags ? help",
        ],
    );

    // D10 the hop editor with an inherited chain, D11 with the picker open.
    let mut f = fleet(10, true);
    add_db9(&mut f.app);
    set_group_jump(&f.app, "db", Some("jump-2"));
    f.app.reload_hosts().unwrap();
    open_form(&mut f.app, "db-9");
    check(
        "D10Inherited",
        "D10Inherited",
        &render(&f.app),
        &["inherited from group \"prod/db\" · add a hop to override"],
    );
    let mut f = fleet(10, true);
    add_db9(&mut f.app);
    open_form(&mut f.app, "db-9");
    f.app.host_form.as_mut().unwrap().proxy_jump = "bastion-eu,jump-2".into();
    press(&mut f.app, 'a');
    check(
        "D11PickerOpen",
        "D11PickerOpen",
        &render(&f.app),
        &[
            "↑↓ choose ↵ add hop / use route End raw hop Esc close",
            "path ◉ you ━▶ ● bastion-eu ━▶ ● jump-2 ━▶ db-9",
            "reuse a route (replaces all hops)",
            "+ raw user@host:port End",
            "↵ add End raw hop Esc close",
            "+ a saved host r raw │ ↵ replace d remove Ctrl+↑↓ move",
        ],
    );

    // D12: a map test in progress (a socket that never answers).
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    let mut f = fleet(10, true);
    point_host_at(&mut f.app, "bastion-eu", port);
    to_map(&mut f.app);
    down(&mut f.app, 1);
    f.app.handle_key(key(KeyCode::Right)).unwrap();
    f.app.handle_key(key(KeyCode::Right)).unwrap();
    press(&mut f.app, 't');
    f.app.poll_test_run();
    check(
        "D12InProgress",
        "D12InProgress",
        &render(&f.app),
        &[TREE_ROW, "─ steps ─", LEGEND_EDGE],
    );

    // D13: the hop editor after a failed test of its chain.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut f = fleet(10, true);
    add_db9(&mut f.app);
    point_host_at(&mut f.app, "bastion-eu", closed);
    open_form(&mut f.app, "db-9");
    f.app.host_form.as_mut().unwrap().proxy_jump = "bastion-eu,jump-2".into();
    press(&mut f.app, 't');
    for _ in 0..200 {
        f.app.poll_test_run();
        if f.app.test_run.as_ref().is_some_and(|r| !r.running()) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    check(
        "D13Failed",
        "D13Failed",
        &render(&f.app),
        &["✗ failed", "- skipped"],
    );

    // The same default screen at 155x31, the maintainer's terminal.
    let mut f = fleet(188, true);
    f.app.terminal_area = ratatui::layout::Rect::new(0, 0, 155, 31);
    press(&mut f.app, '6');
    dump("T155x31", &grid(&f.app, 155, 31));
    let mut f = fleet(69, false);
    f.app.terminal_area = ratatui::layout::Rect::new(0, 0, 155, 31);
    press(&mut f.app, '6');
    dump("T155x31-empty", &grid(&f.app, 155, 31));
}
