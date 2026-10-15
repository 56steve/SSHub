//! Network map tab (issue #79): tab 6, arrow navigation, the footer, and that
//! no hosts-tab key leaks through to the hidden host list.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;
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
    store.create_host(&mk("db-1", Some("jump-2"))).unwrap();
    store.create_host(&mk("db-2", Some("jump-2"))).unwrap();
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

    // Down to the jump-2 row, right onto a glyph: the ribbon names the host.
    app.handle_key(key(KeyCode::Down)).unwrap();
    app.handle_key(key(KeyCode::Right)).unwrap();
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

#[test]
fn enter_on_a_cycle_host_refuses_instead_of_connecting() {
    let store = Arc::new(LauncherStore::open_in_memory().unwrap());
    for (n, j) in [("loop-a", "loop-b"), ("loop-b", "loop-a")] {
        let mut h = NewHost::launcher(n, format!("{n}.example"));
        h.proxy_jump = Some(j.into());
        store.create_host(&h).unwrap();
    }
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
    app.handle_key(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::empty()))
        .unwrap();
    app.handle_key(key(KeyCode::Enter)).unwrap();
    assert!(app.sessions.is_empty());
    assert!(app
        .host_notice
        .as_deref()
        .is_some_and(|n| n.contains("jump loop")));
}
