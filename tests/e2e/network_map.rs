//! Network map tab (issue #79): tab 6, arrow navigation, the footer, and that
//! no hosts-tab key leaks through to the hidden host list.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;
use sshub::app::network_map::{Lens, MapSel};
use sshub::app::{App, AppDeps, AppMode};
use sshub::config::AppConfig;
use sshub::metadata::MetadataDb;
use sshub::ssh::{HostResolver, SshHost};
use sshub::store::{LauncherStore, NewHost};

struct EmptyResolver;

impl HostResolver for EmptyResolver {
    fn list_hosts(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![])
    }

    fn resolve_host(&self, name: &str) -> anyhow::Result<SshHost> {
        Ok(SshHost::new(name))
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

fn app() -> App {
    let store = Arc::new(LauncherStore::open_in_memory().unwrap());
    let mk = |name: &str, jump: Option<&str>| {
        let mut h = NewHost::launcher(name, format!("{name}.example"));
        h.proxy_jump = jump.map(String::from);
        h
    };
    store.create_host(&mk("bastion-eu", None)).unwrap();
    store
        .create_host(&mk("jump-2", Some("bastion-eu")))
        .unwrap();
    let mut db1 = mk("db-1", Some("jump-2"));
    db1.tags = vec!["db".into(), "pci".into()];
    let db1 = store.create_host(&db1).unwrap();
    let mut db2 = mk("db-2", Some("jump-2"));
    db2.tags = vec!["db".into()];
    store.create_host(&db2).unwrap();
    store
        .create_tunnel(&sshub::store::NewTunnel {
            host_id: Some(db1.id),
            tunnel_type: sshub::store::TunnelType::Local,
            local_port: 5432,
            remote_host: "localhost".into(),
            remote_port: 5432,
            label: None,
            auto_connect: false,
        })
        .unwrap();
    store.create_host(&mk("standalone", None)).unwrap();
    let mut app = App::new_with_deps(
        AppConfig::default(),
        AppDeps {
            resolver: Box::new(EmptyResolver),
            metadata: Arc::new(MetadataDb::default()),
            store,
            password_store: Box::new(sshub::credentials::NoopPasswordStore),
        },
    );
    app.reload_hosts().unwrap();
    app
}

fn draw(app: &App, w: u16, h: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| sshub::tui::render(f, app)).unwrap();
    terminal.backend().buffer().clone()
}

fn screen(buf: &Buffer) -> String {
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn tab_six_shows_the_jump_tree_and_the_map_footer() {
    let mut app = app();
    app.handle_key(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::empty()))
        .unwrap();
    assert_eq!(app.active_tab, 5);
    let text = screen(&draw(&app, 160, 45));
    for needle in [
        "map",
        "hosts behind this hop",
        "bastion-eu",
        "jump-2",
        "direct-only",
        "path ribbon",
        "along the path",
    ] {
        assert!(text.contains(needle), "missing {needle:?}:\n{text}");
    }
}

#[test]
fn arrows_walk_the_tree_and_hosts_tab_keys_do_not_leak() {
    let mut app = app();
    app.handle_key(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::empty()))
        .unwrap();
    let hosts_before = app.hosts.len();
    let selected_before = app.selected;

    // The biggest subtree (jump-2) is the first row; right onto its glyph: the
    // ribbon names the host.
    app.handle_key(key(KeyCode::Right)).unwrap();
    let text = screen(&draw(&app, 160, 45));
    assert!(text.contains("\u{25c2} db-1"), "{text}");

    // `d` (delete host), `a` (add host), `j` (hosts-tab down) and Shift+Z do
    // nothing on the map: no mode change, no host removed, no selection moved.
    for c in ['d', 'a', 'j', 'Z', 'b'] {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()))
            .unwrap();
        assert_eq!(app.mode, AppMode::Normal, "key {c:?} opened an overlay");
    }
    assert_eq!(app.hosts.len(), hosts_before);
    assert_eq!(app.selected, selected_before);
    assert_eq!(app.active_tab, 5);

    // 1 returns to hosts.
    app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::empty()))
        .unwrap();
    assert_eq!(app.active_tab, 0);
}

/// ssh_config hosts that jump through each other: ssh follows a config hop's
/// own `ProxyJump`, so only these can form a loop.
struct LoopResolver;

impl HostResolver for LoopResolver {
    fn list_hosts(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec!["loop-a".into(), "loop-b".into()])
    }

    fn resolve_host(&self, name: &str) -> anyhow::Result<SshHost> {
        let mut h = SshHost::new(name);
        h.hostname = Some(format!("{name}.example"));
        h.port = Some(22);
        h.proxy_jump = Some(if name == "loop-a" { "loop-b" } else { "loop-a" }.into());
        Ok(h)
    }
}

#[test]
fn enter_on_a_cycle_host_refuses_instead_of_connecting() {
    let store = Arc::new(LauncherStore::open_in_memory().unwrap());
    for n in ["loop-a", "loop-b"] {
        let mut h = NewHost::launcher(n, format!("{n}.example"));
        h.source = sshub::store::HostSource::SshConfig;
        store.create_host(&h).unwrap();
    }
    let mut app = App::new_with_deps(
        AppConfig::default(),
        AppDeps {
            resolver: Box::new(LoopResolver),
            metadata: Arc::new(MetadataDb::default()),
            store,
            password_store: Box::new(sshub::credentials::NoopPasswordStore),
        },
    );
    app.reload_hosts().unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::empty()))
        .unwrap();
    app.handle_key(key(KeyCode::Enter)).unwrap();
    assert!(app.sessions.is_empty());
    assert!(app
        .host_notice
        .as_deref()
        .is_some_and(|n| n.contains("jump loop")));
}

fn press(app: &mut App, c: char) {
    let mods = if c.is_uppercase() {
        KeyModifiers::SHIFT
    } else {
        KeyModifiers::empty()
    };
    app.handle_key(KeyEvent::new(KeyCode::Char(c), mods))
        .unwrap();
}

fn to_map(app: &mut App) {
    press(app, '6');
    assert_eq!(app.active_tab, 5);
}

#[test]
fn blast_lens_walks_dependents_and_broadcasts_to_them() {
    let mut app = app();
    to_map(&mut app);
    // Row 0 is the biggest subtree: jump-2, with db-1 and db-2 behind it.
    press(&mut app, 'B');
    let Some(Lens::Blast(b)) = app.map_view.as_ref().map(|v| &v.lens) else {
        panic!("blast lens not active");
    };
    assert_eq!(b.hosts.len(), 2);
    let text = screen(&draw(&app, 160, 45));
    assert!(
        text.contains("blast radius \u{b7} 2 hosts depend on jump-2"),
        "{text}"
    );

    press(&mut app, 'n');
    let first = app.map_view.as_ref().unwrap().selection();
    assert!(matches!(first, Some(MapSel::Host(_))));
    press(&mut app, 'n');
    let second = app.map_view.as_ref().unwrap().selection();
    assert_ne!(first, second);
    press(&mut app, 'N');
    assert_eq!(app.map_view.as_ref().unwrap().selection(), first);

    // b: broadcast wizard on exactly the dependents, at the command prompt.
    press(&mut app, 'b');
    assert_eq!(app.mode, AppMode::BroadcastCommand);
    assert_eq!(app.broadcast_setup.as_ref().unwrap().candidates.len(), 2);
    app.handle_key(key(KeyCode::Esc)).unwrap();
    app.handle_key(key(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, AppMode::Normal);

    // Esc clears the lens, a second Esc has nothing left to clear.
    app.handle_key(key(KeyCode::Esc)).unwrap();
    assert!(matches!(app.map_view.as_ref().unwrap().lens, Lens::None));
}

#[test]
fn filter_grammar_hides_rows_and_esc_restores_them() {
    let mut app = app();
    to_map(&mut app);
    let all = app.map_view.as_ref().unwrap().rows.len();
    press(&mut app, '/');
    assert!(app.map_view.as_ref().unwrap().editing);
    // Digits typed into the filter must not switch tabs.
    for c in "tag:db 6".chars() {
        press(&mut app, c);
    }
    assert_eq!(app.active_tab, 5);
    app.handle_key(key(KeyCode::Backspace)).unwrap();
    app.handle_key(key(KeyCode::Backspace)).unwrap();
    app.handle_key(key(KeyCode::Enter)).unwrap();
    let v = app.map_view.as_ref().unwrap();
    assert!(!v.editing);
    assert_eq!(v.query, "tag:db");
    assert!(v.rows.len() < all, "{} < {all}", v.rows.len());
    let text = screen(&draw(&app, 160, 45));
    assert!(text.contains("/ tag:db"), "{text}");
    assert!(text.contains("2 of 5 hosts"), "{text}");

    // The hosts-tab search state is untouched.
    assert!(app.search_query.is_empty());
    app.handle_key(key(KeyCode::Esc)).unwrap();
    let v = app.map_view.as_ref().unwrap();
    assert!(v.query.is_empty());
    assert_eq!(v.rows.len(), all);
}

#[test]
fn tag_picker_feeds_the_filter() {
    let mut app = app();
    to_map(&mut app);
    press(&mut app, '#');
    assert!(app.map_view.as_ref().unwrap().picker.is_some());
    let text = screen(&draw(&app, 160, 45));
    assert!(text.contains("#db") && text.contains("#pci"), "{text}");
    // Items are sorted: db, pci. Pick pci.
    app.handle_key(key(KeyCode::Down)).unwrap();
    app.handle_key(key(KeyCode::Enter)).unwrap();
    let v = app.map_view.as_ref().unwrap();
    assert!(v.picker.is_none());
    assert_eq!(v.query, "tag:pci");
    assert_eq!(v.matched.as_ref().unwrap().len(), 1);
}

#[test]
fn tunnel_lens_marks_hosts_with_tunnels() {
    let mut app = app();
    to_map(&mut app);
    press(&mut app, 'o');
    assert!(matches!(app.map_view.as_ref().unwrap().lens, Lens::Tunnel));
    let text = screen(&draw(&app, 160, 45));
    assert!(text.contains("tunnel lens"), "{text}");
    assert!(text.contains('\u{21c4}'), "{text}");
    // x on a host with no tunnel says so instead of doing anything.
    press(&mut app, 'x');
    assert_eq!(app.host_notice.as_deref(), Some("no tunnel on this host"));
    press(&mut app, 'o');
    assert!(matches!(app.map_view.as_ref().unwrap().lens, Lens::None));
}
