//! The structured hop editor in the host form, and the map's `u` action
//! (issue #79). Everything goes through `App::handle_key`.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use sshub::app::{App, AppDeps, AppMode, HostFormField};
use sshub::config::AppConfig;
use sshub::metadata::MetadataDb;
use sshub::ssh::{HostResolver, SshHost};
use sshub::store::{LauncherStore, NewHost, NewHostGroup};

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

fn press(app: &mut App, c: char) {
    let mods = if c.is_uppercase() {
        KeyModifiers::SHIFT
    } else {
        KeyModifiers::empty()
    };
    app.handle_key(KeyEvent::new(KeyCode::Char(c), mods))
        .unwrap();
}

fn typed(app: &mut App, s: &str) {
    for c in s.chars() {
        press(app, c);
    }
}

fn app(group_jump: Option<&str>) -> App {
    let store = Arc::new(LauncherStore::open_in_memory().unwrap());
    let group = store
        .create_group(&NewHostGroup {
            name: "prod".into(),
            default_proxy_jump: group_jump.map(String::from),
            ..Default::default()
        })
        .unwrap();
    for (n, jump) in [
        ("bastion-eu", None),
        ("bastion-us", None),
        ("db-1", Some("bastion-eu")),
    ] {
        let mut h = NewHost::launcher(n, format!("{n}.example"));
        h.proxy_jump = jump.map(String::from);
        h.group_id = Some(group.id);
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
    app
}

/// Open the edit form of `name` with the cursor on the ProxyJump field.
fn edit(app: &mut App, name: &str) {
    let idx = app.hosts.iter().position(|h| h.name() == name).unwrap();
    assert!(app.reveal_host(idx));
    app.handle_key(key(KeyCode::Char('e'))).unwrap();
    assert_eq!(app.mode, AppMode::HostForm);
    app.host_form.as_mut().unwrap().field = HostFormField::ProxyJump;
}

fn jump(app: &App) -> String {
    app.host_form.as_ref().unwrap().proxy_jump.clone()
}

fn screen(app: &App) -> String {
    let mut t = Terminal::new(TestBackend::new(160, 60)).unwrap();
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
fn picker_adds_replaces_moves_and_removes_hops_saved_as_the_plain_string() {
    let mut app = app(None);
    edit(&mut app, "db-1");
    assert_eq!(jump(&app), "bastion-eu");
    let text = screen(&app);
    assert!(text.contains("hops \u{2500} own value"), "{text}");
    assert!(text.contains("saves as: bastion-eu"), "{text}");

    // a: picker, type to filter, Enter adds after the selected hop.
    press(&mut app, 'a');
    typed(&mut app, "bastion-us");
    assert!(screen(&app).contains("one saved host"));
    // Routes come first when any match; pick the host row with Down until it
    // is a host (the filter leaves just the host).
    app.handle_key(key(KeyCode::Enter)).unwrap();
    assert_eq!(jump(&app), "bastion-eu,bastion-us");

    // Ctrl+Up moves the selected hop up.
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL))
        .unwrap();
    assert_eq!(jump(&app), "bastion-us,bastion-eu");

    // d removes the selected hop, X clears to inherit, n stores `none`.
    press(&mut app, 'd');
    assert_eq!(jump(&app), "bastion-eu");
    press(&mut app, 'n');
    assert_eq!(jump(&app), "none");
    assert!(screen(&app).contains("saves as: none"));
    press(&mut app, 'X');
    assert_eq!(jump(&app), "");

    // Letters never leak into the value, and Tab still leaves the field.
    press(&mut app, 'z');
    assert_eq!(jump(&app), "");
    app.handle_key(key(KeyCode::Tab)).unwrap();
    assert_ne!(
        app.host_form.as_ref().unwrap().field,
        HostFormField::ProxyJump
    );
}

#[test]
fn route_reuse_replaces_every_hop() {
    let mut app = app(None);
    edit(&mut app, "bastion-us");
    assert_eq!(jump(&app), "");
    press(&mut app, 'a');
    // The only route in use is `bastion-eu`, listed first.
    assert!(screen(&app).contains("reuse a route"));
    app.handle_key(key(KeyCode::Enter)).unwrap();
    assert_eq!(jump(&app), "bastion-eu");
}

#[test]
fn hop_that_would_loop_is_refused() {
    let mut app = app(None);
    edit(&mut app, "bastion-eu");
    // db-1 depends on bastion-eu: as a hop of bastion-eu it would loop.
    press(&mut app, 'a');
    typed(&mut app, "db-1");
    app.handle_key(key(KeyCode::Enter)).unwrap();
    assert_eq!(jump(&app), "");
    assert!(screen(&app).contains("would loop"));
    // The host itself too.
    app.handle_key(key(KeyCode::Esc)).unwrap();
    press(&mut app, 'a');
    typed(&mut app, "bastion-eu");
    app.handle_key(key(KeyCode::Enter)).unwrap();
    assert_eq!(jump(&app), "");
}

#[test]
fn raw_hop_is_validated_and_option_like_input_is_refused() {
    let mut app = app(None);
    edit(&mut app, "bastion-us");
    press(&mut app, 'r');
    typed(&mut app, "-oProxyCommand=id");
    app.handle_key(key(KeyCode::Enter)).unwrap();
    assert_eq!(jump(&app), "", "an option-like hop must not be added");
    assert!(screen(&app).contains("not a valid"));
    for _ in 0..30 {
        app.handle_key(key(KeyCode::Backspace)).unwrap();
    }
    typed(&mut app, "ops@203.0.113.7:2222");
    app.handle_key(key(KeyCode::Enter)).unwrap();
    assert_eq!(jump(&app), "ops@203.0.113.7:2222");
}

#[test]
fn invalid_stored_hop_blocks_save_with_a_notice() {
    let mut app = app(None);
    edit(&mut app, "bastion-us");
    app.host_form.as_mut().unwrap().proxy_jump = "-oProxyCommand=id".into();
    app.handle_key(key(KeyCode::F(2))).unwrap();
    assert_eq!(app.mode, AppMode::HostForm, "save must be refused");
    assert!(app
        .host_notice
        .as_deref()
        .unwrap_or("")
        .contains("ProxyJump hop"));
}

#[test]
fn inherited_chain_is_shown_muted_and_none_overrides_the_group() {
    let mut app = app(Some("bastion-us"));
    edit(&mut app, "bastion-eu");
    assert_eq!(jump(&app), "");
    let text = screen(&app);
    assert!(text.contains("hops \u{2500} inherited"), "{text}");
    assert!(text.contains("inherits \"bastion-us\""), "{text}");
    press(&mut app, 'n');
    assert!(screen(&app).contains("direct (overrides inherited)"));
    // Save: the host stores `none`, and its effective chain is empty.
    app.handle_key(key(KeyCode::F(2))).unwrap();
    let stored = app.store().get_host_by_name("bastion-eu").unwrap().unwrap();
    assert_eq!(stored.proxy_jump.as_deref(), Some("none"));
}

#[test]
fn map_use_as_jump_replaces_or_appends_and_refuses_loops() {
    let mut app = app(None);
    press(&mut app, '6');
    // Cursor on the first row: bastion-eu (biggest subtree). u opens a picker.
    press(&mut app, 'u');
    assert!(app.map_view.as_ref().unwrap().use_picker.is_some());
    // The hop itself is greyed: choosing it is refused.
    typed(&mut app, "bastion-eu");
    app.handle_key(key(KeyCode::Enter)).unwrap();
    assert!(app.host_notice.as_deref().unwrap_or("").contains("loop"));
    for _ in 0..10 {
        app.handle_key(key(KeyCode::Backspace)).unwrap();
    }
    typed(&mut app, "bastion-us");
    // Tab toggles append; bastion-us has no jump yet so the result is equal.
    app.handle_key(key(KeyCode::Tab)).unwrap();
    app.handle_key(key(KeyCode::Enter)).unwrap();
    let h = app.store().get_host_by_name("bastion-us").unwrap().unwrap();
    assert_eq!(h.proxy_jump.as_deref(), Some("bastion-eu"));
    assert!(app.map_view.as_ref().unwrap().use_picker.is_none());
}

#[test]
fn ssh_config_hosts_get_no_editor_keys() {
    let mut app = app(None);
    edit(&mut app, "db-1");
    app.host_form.as_mut().unwrap().metadata_only = true;
    // The chain is listed (read-only), with no editing hints.
    let text = screen(&app);
    assert!(
        text.contains("hops \u{2500} from ssh_config, read-only"),
        "{text}"
    );
    assert!(!text.contains("+ a saved host"), "{text}");
    press(&mut app, 'a');
    press(&mut app, 'd');
    assert!(app.hop_editor.picker.is_none());
    assert_eq!(jump(&app), "bastion-eu");
}
