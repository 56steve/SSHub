//! The map against `docs/network-map/mockups/D*.txt` (issue #79).
//!
//! `fleet()` is the fictional fleet of the mockups (bastion-eu, jump-2,
//! bastion-us, corp-gw, a raw hop, a proxy cmd host, a loop, 188 direct hosts).
//! The key-row assertions below check what the mockups show; the ignored
//! `dump_mockup_scenarios` test writes every state as a grid to the directory in
//! `MAP_DUMP_DIR`, to compare with the `.txt` files by eye or `diff`.

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

fn text(app: &App, w: u16, h: u16) -> String {
    grid(app, w, h).join("\n")
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

#[test]
#[ignore = "writes the mockup scenarios to MAP_DUMP_DIR for comparison"]
fn dump_mockup_scenarios() {
    let size = (160, 45);
    let render = |app: &App, name: &str| dump(name, &grid(app, size.0, size.1));

    // D01 default, D02 selected glyph, D03 blast, D04 filter, D05 tunnel lens.
    let mut f = fleet(188, true);
    to_map(&mut f.app);
    render(&f.app, "D01Default");
    down(&mut f.app, 1);
    f.app.handle_key(key(KeyCode::Right)).unwrap();
    f.app.handle_key(key(KeyCode::Right)).unwrap();
    render(&f.app, "D02SelectedDetail");

    let mut f = fleet(188, true);
    to_map(&mut f.app);
    press(&mut f.app, 'B');
    render(&f.app, "D03BlastRadius");

    let mut f = fleet(188, true);
    to_map(&mut f.app);
    press(&mut f.app, '/');
    type_str(&mut f.app, "tag:db");
    f.app.handle_key(key(KeyCode::Enter)).unwrap();
    render(&f.app, "D04Filter");

    let mut f = fleet(188, true);
    to_map(&mut f.app);
    down(&mut f.app, 1);
    f.app.handle_key(key(KeyCode::Right)).unwrap();
    press(&mut f.app, 'o');
    render(&f.app, "D05TunnelLens");

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
    render(&f.app, "D06EdgeCases");

    // D07 empty, D14 direct-only expanded (Shift+Z).
    let mut f = fleet(214, false);
    to_map(&mut f.app);
    render(&f.app, "D07Empty");
    press(&mut f.app, 'Z');
    render(&f.app, "D14DirectOnlyExpanded");

    // D09 at 100x30.
    let mut f = fleet(188, true);
    f.app.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
    press(&mut f.app, '6');
    down(&mut f.app, 1);
    f.app.handle_key(key(KeyCode::Right)).unwrap();
    f.app.handle_key(key(KeyCode::Right)).unwrap();
    dump("D09Narrow100x30", &grid(&f.app, 100, 30));

    // D08: 212 jump hosts, 1,840 hosts, clustered.
    let mut f = big_fleet();
    to_map(&mut f.app);
    render(&f.app, "D08ManyJumpHosts");

    // D04 while the filter line is still open (the mockup's moment).
    let mut f = fleet(188, true);
    to_map(&mut f.app);
    press(&mut f.app, '/');
    type_str(&mut f.app, "tag:db");
    render(&f.app, "D04FilterTyping");

    // D10 the hop editor with an inherited chain, D11 with the picker open.
    let mut f = fleet(10, true);
    add_db9(&mut f.app);
    set_group_jump(&f.app, "db", Some("jump-2"));
    f.app.reload_hosts().unwrap();
    open_form(&mut f.app, "db-9");
    render(&f.app, "D10Inherited");
    let mut f = fleet(10, true);
    add_db9(&mut f.app);
    open_form(&mut f.app, "db-9");
    f.app.host_form.as_mut().unwrap().proxy_jump = "bastion-eu,jump-2".into();
    press(&mut f.app, 'a');
    render(&f.app, "D11PickerOpen");

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
    std::thread::sleep(std::time::Duration::from_millis(300));
    f.app.poll_test_run();
    render(&f.app, "D12InProgress");

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
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    render(&f.app, "D13Failed");

    // The same default screen at 155x31, the maintainer's terminal.
    let mut f = fleet(188, true);
    f.app.terminal_area = ratatui::layout::Rect::new(0, 0, 155, 31);
    press(&mut f.app, '6');
    dump("T155x31", &grid(&f.app, 155, 31));
    let mut f = fleet(69, false);
    f.app.terminal_area = ratatui::layout::Rect::new(0, 0, 155, 31);
    press(&mut f.app, '6');
    dump("T155x31-empty", &grid(&f.app, 155, 31));
    let _ = text;
}
