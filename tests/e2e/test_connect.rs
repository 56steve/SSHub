//! Test connect from the map (`t`) and the hop editor (issue #79), against real
//! `ssh` and loopback sockets. The SSH-level reason mapping has its own oracle
//! tests next to `src/ssh/test_connect.rs`; this is the wiring.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use sshub::app::{App, AppDeps, HostFormField};
use sshub::config::AppConfig;
use sshub::metadata::MetadataDb;
use sshub::ssh::test_connect::StepState;
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

fn have_ssh() -> bool {
    super::support::isolate_home();
    std::process::Command::new("ssh").arg("-V").output().is_ok()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty())
}

/// `db` sits at `db_port` on loopback; `jump` is a launcher host at `jump_port`.
fn app(db_port: u16, jump_port: u16) -> App {
    let store = Arc::new(LauncherStore::open_in_memory().unwrap());
    let mut jump = NewHost::launcher("jump", "127.0.0.1");
    jump.port = Some(jump_port);
    jump.username = Some("nobody".into());
    store.create_host(&jump).unwrap();
    let mut db = NewHost::launcher("db", "127.0.0.1");
    db.port = Some(db_port);
    db.username = Some("nobody".into());
    db.proxy_jump = Some("jump".into());
    store.create_host(&db).unwrap();
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

fn wait_done(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(40);
    while app.test_run.as_ref().is_some_and(|r| r.running()) {
        assert!(Instant::now() < deadline, "test connect never finished");
        app.poll_test_run();
        std::thread::sleep(Duration::from_millis(30));
    }
    app.poll_test_run();
}

fn screen(app: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| sshub::tui::render(f, app)).unwrap();
    let buf = t.backend().buffer().clone();
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
fn map_test_stops_at_the_first_failing_hop_and_shows_it_on_the_ribbon() {
    if !have_ssh() {
        return;
    }
    // Nobody listens on either port: the first hop (jump) refuses, and the
    // second step never runs.
    let mut app = app(free_port(), free_port());
    app.handle_key(key('6')).unwrap();
    // Row 0 is the jump hop; right once is the glyph of `db` behind it.
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::empty()))
        .unwrap();
    app.handle_key(key('t')).unwrap();
    assert!(app.test_run.is_some());
    wait_done(&mut app);
    let run = app.test_run.as_ref().unwrap();
    assert_eq!(run.states.len(), 2);
    assert!(
        matches!(&run.states[0], StepState::Failed { reason, .. } if reason == "refused"),
        "{:?}",
        run.states
    );
    assert_eq!(
        run.states[1],
        StepState::Skipped {
            reason: "skipped".into()
        },
        "no step after a failure: skipped, not cancelled"
    );

    let text = screen(&app, 160, 45);
    assert!(text.contains("failed at hop 1 of 2 (refused)"), "{text}");
    assert!(text.contains("\u{2717} refused"), "{text}");
    assert!(
        text.contains("known hosts") || text.contains("t again"),
        "{text}"
    );
    // The failed hop is down only because the test said so.
    assert!(text.contains("\u{d7} jump"), "{text}");

    // Esc closes the finished test.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
        .unwrap();
    assert!(app.test_run.is_none());
}

#[test]
fn esc_cancels_a_running_test_and_kills_it() {
    if !have_ssh() {
        return;
    }
    // A socket that accepts and never speaks keeps ssh waiting.
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    let mut app = app(free_port(), port);
    app.handle_key(key('6')).unwrap();
    app.handle_key(key('t')).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
        .unwrap();
    let started = Instant::now();
    wait_done(&mut app);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancel took {:?}",
        started.elapsed()
    );
    let run = app.test_run.as_ref().unwrap();
    assert!(
        run.states.contains(&StepState::Cancelled),
        "{:?}",
        run.states
    );
}

#[test]
fn hop_editor_t_tests_the_chain_as_it_is_in_the_form() {
    if !have_ssh() {
        return;
    }
    let mut app = app(free_port(), free_port());
    let idx = app.hosts.iter().position(|h| h.name() == "db").unwrap();
    assert!(app.reveal_host(idx));
    app.handle_key(key('e')).unwrap();
    app.host_form.as_mut().unwrap().field = HostFormField::ProxyJump;
    app.handle_key(key('t')).unwrap();
    wait_done(&mut app);
    let text = screen(&app, 160, 70);
    assert!(text.contains("test chain"), "{text}");
    assert!(text.contains("failed at hop 1 of 2"), "{text}");
    assert!(text.contains("refused"), "{text}");
    assert!(text.contains("Save still allowed"), "{text}");
    // Saving is not blocked by the failed test.
    app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::empty()))
        .unwrap();
    assert!(app.host_form.is_none());
}

#[test]
fn failed_test_offers_known_hosts_push_key_and_ssh_log() {
    if !have_ssh() {
        return;
    }
    let mut app = app(free_port(), free_port());
    // A key identity so the push-key picker has something to offer.
    app.store()
        .create_identity(&sshub::store::NewIdentity {
            name: "k".into(),
            username: Some("u".into()),
            private_key: Some("/tmp/does-not-matter".into()),
            ..Default::default()
        })
        .unwrap();
    app.handle_key(key('6')).unwrap();
    app.handle_key(key('t')).unwrap();
    wait_done(&mut app);
    let text = screen(&app, 160, 45);
    assert!(text.contains("H known hosts"), "{text}");
    assert!(text.contains("P push key"), "{text}");
    assert!(text.contains("l ssh log"), "{text}");

    // The failure is in the ssh log of the hop that failed.
    assert!(app
        .ssh_log
        .iter()
        .any(|e| e.host_name == "jump" && e.line.contains("refused")));

    // l: the hosts tab with that host's log, zoomed.
    app.handle_key(key('l')).unwrap();
    assert_eq!(app.active_tab, 0);
    assert!(app.panel_zoomed);
    assert_eq!(app.focused_panel, sshub::app::PanelId::SshLog);
    assert_eq!(app.selected_entry().unwrap().name(), "jump");
    app.panel_zoomed = false;

    // P: the identity picker, aimed at the failed hop.
    app.handle_key(key('6')).unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT))
        .unwrap();
    assert_eq!(app.mode, sshub::app::AppMode::PushKeyIdentityPicker);
    assert_eq!(app.selected_entry().unwrap().name(), "jump");
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
        .unwrap();

    // H: known hosts, and Esc comes back to the map.
    app.handle_key(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::SHIFT))
        .unwrap();
    assert_eq!(app.mode, sshub::app::AppMode::KnownHosts);
}

#[test]
fn hop_editor_h_opens_known_hosts_and_returns_to_the_form() {
    if !have_ssh() {
        return;
    }
    let mut app = app(free_port(), free_port());
    let idx = app.hosts.iter().position(|h| h.name() == "db").unwrap();
    assert!(app.reveal_host(idx));
    app.handle_key(key('e')).unwrap();
    app.host_form.as_mut().unwrap().field = HostFormField::ProxyJump;
    app.handle_key(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::SHIFT))
        .unwrap();
    assert_eq!(app.mode, sshub::app::AppMode::KnownHosts);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))
        .unwrap();
    assert_eq!(app.mode, sshub::app::AppMode::HostForm);
    assert_eq!(app.host_form.as_ref().unwrap().proxy_jump, "jump");
}
