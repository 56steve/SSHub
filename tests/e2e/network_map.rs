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
        "blast",
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
    assert!(text.contains(" hosts match"), "{text}");

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

fn render_frame(app: &App) {
    let mut t = Terminal::new(TestBackend::new(160, 45)).unwrap();
    t.draw(|f| sshub::tui::render(f, app)).unwrap();
}

fn mouse(app: &mut App, kind: crossterm::event::MouseEventKind, x: u16, y: u16) {
    app.handle_mouse(crossterm::event::MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::empty(),
    })
    .unwrap();
}

fn click(app: &mut App, x: u16, y: u16) {
    use crossterm::event::{MouseButton, MouseEventKind::*};
    mouse(app, Down(MouseButton::Left), x, y);
    mouse(app, Up(MouseButton::Left), x, y);
}

#[test]
fn clicking_selects_a_hop_or_glyph_and_the_wheel_scrolls() {
    use sshub::app::ClickHit;
    let mut app = app();
    app.terminal_area = ratatui::layout::Rect::new(0, 0, 160, 45);
    to_map(&mut app);
    render_frame(&app);

    // Find a glyph cell the frame recorded (a slot past the row's hops).
    let (area, row, pos) = {
        let v = app.map_view.as_ref().unwrap();
        let map = app.click_map.borrow();
        map.iter()
            .rev()
            .find_map(|t| match t.hit {
                ClickHit::MapCell { row, pos } if pos >= v.hop_count(row) && t.area.width == 1 => {
                    Some((t.area, row, pos))
                }
                _ => None,
            })
            .expect("the map records its glyphs")
    };
    click(&mut app, area.x, area.y);
    let v = app.map_view.as_ref().unwrap();
    assert_eq!((v.row, v.pos), (row, pos));
    assert!(matches!(v.selection(), Some(MapSel::Host(_))));

    // A click on a row's empty space selects that row's own hop.
    render_frame(&app);
    let (r0, y0) = {
        let map = app.click_map.borrow();
        let t = map
            .iter()
            .find(|t| matches!(t.hit, ClickHit::MapCell { row: 0, .. }) && t.area.width > 1)
            .unwrap();
        (t.area.x + 1, t.area.y)
    };
    click(&mut app, r0, y0);
    let v = app.map_view.as_ref().unwrap();
    assert_eq!(v.row, 0);
    assert!(matches!(v.selection(), Some(MapSel::Hop(_))));

    // Wheel moves the cursor row.
    let body_y = 10;
    mouse(
        &mut app,
        crossterm::event::MouseEventKind::ScrollDown,
        20,
        body_y,
    );
    assert!(app.map_view.as_ref().unwrap().row > 0);
    mouse(
        &mut app,
        crossterm::event::MouseEventKind::ScrollUp,
        20,
        body_y,
    );
    mouse(
        &mut app,
        crossterm::event::MouseEventKind::ScrollUp,
        20,
        body_y,
    );
    assert_eq!(app.map_view.as_ref().unwrap().row, 0);
}

#[test]
fn double_click_connects_through_the_same_path_as_enter() {
    use sshub::app::ClickHit;
    // A jump loop is the one target Enter refuses without spawning ssh.
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
    app.terminal_area = ratatui::layout::Rect::new(0, 0, 160, 45);
    to_map(&mut app);
    render_frame(&app);
    let (x, y) = {
        let map = app.click_map.borrow();
        let t = map
            .iter()
            .rev()
            .find(|t| matches!(t.hit, ClickHit::MapCell { .. }) && t.area.width == 1)
            .expect("a loop host glyph");
        (t.area.x, t.area.y)
    };
    click(&mut app, x, y);
    assert!(app.host_notice.is_none(), "one click only selects");
    click(&mut app, x, y);
    assert!(app.sessions.is_empty());
    assert!(app
        .host_notice
        .as_deref()
        .is_some_and(|n| n.contains("jump loop")));
}

#[test]
fn y_copies_the_command_a_saves_a_raw_hop_and_z_zooms_the_map() {
    let store = Arc::new(LauncherStore::open_in_memory().unwrap());
    let mut h = NewHost::launcher("legacy", "legacy.example");
    h.proxy_jump = Some("ops@203.0.113.7:2222".into());
    store.create_host(&h).unwrap();
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
    to_map(&mut app);

    // y on the raw hop copies `ssh <hop>`; on the host behind it, the -J form.
    // (Captured: the escape sequence must not reach the developer's terminal.)
    let ((), copied) = sshub::osc52::capture(|| {
        press(&mut app, 'y');
        app.handle_key(key(KeyCode::Right)).unwrap();
        press(&mut app, 'y');
    });
    assert_eq!(
        copied,
        [
            "ssh -p 2222 'ops@203.0.113.7'",
            "ssh -J 'ops@203.0.113.7:2222' -o 'ForwardAgent=no' legacy.example"
        ]
    );
    assert_eq!(
        app.host_notice.as_deref(),
        Some("copied: ssh -J 'ops@203.0.113.7:2222' -o 'ForwardAgent=no' legacy.example")
    );
    // a is only for a raw hop.
    press(&mut app, 'a');
    assert_eq!(app.mode, AppMode::Normal);
    app.handle_key(key(KeyCode::Left)).unwrap();

    // z zooms the map over the ribbon, Esc brings it back.
    press(&mut app, 'z');
    assert!(app.map_view.as_ref().unwrap().zoomed);
    assert!(!screen(&draw(&app, 160, 45)).contains("path ribbon"));
    app.handle_key(key(KeyCode::Esc)).unwrap();
    assert!(!app.map_view.as_ref().unwrap().zoomed);
    assert!(screen(&draw(&app, 160, 45)).contains("path ribbon"));

    // a on the raw hop: the new-host form, filled from it.
    press(&mut app, 'a');
    assert_eq!(app.mode, AppMode::HostForm);
    let f = app.host_form.as_ref().unwrap();
    assert_eq!(
        (f.address.as_str(), f.username.as_str(), f.port.as_str()),
        ("203.0.113.7", "ops", "2222")
    );
}

#[test]
fn pickers_do_not_open_where_the_outline_has_no_room_to_draw_them() {
    let mut app = app();
    to_map(&mut app);
    let _ = draw(&app, 70, 20); // outline layout
    press(&mut app, '#');
    assert!(app.map_view.as_ref().unwrap().picker.is_none());
    press(&mut app, 'u');
    assert!(app.map_view.as_ref().unwrap().use_picker.is_none());
    assert!(app
        .host_notice
        .as_deref()
        .unwrap_or("")
        .contains("bigger terminal"));
    // Back at full size they open.
    let _ = draw(&app, 160, 45);
    press(&mut app, '#');
    assert!(app.map_view.as_ref().unwrap().picker.is_some());
}

#[test]
fn y_shell_quotes_every_word_of_the_copied_command() {
    // Names, users and addresses come from imports: `;`, `$()` and a quote must
    // not run or split when the copied line is pasted into a shell.
    let marker = tempfile::tempdir().unwrap().keep().join("pwned");
    let address = format!("db$(touch {}) x;y", marker.display());
    let store = Arc::new(LauncherStore::open_in_memory().unwrap());
    store
        .create_host(&NewHost::launcher("bastion", "10.0.0.1"))
        .unwrap();
    let mut h = NewHost::launcher("evil", address.clone());
    h.username = Some("o'brien".into());
    h.proxy_jump = Some("bastion".into());
    store.create_host(&h).unwrap();
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
    to_map(&mut app);
    app.handle_key(key(KeyCode::Right)).unwrap(); // the glyph of `evil`
    let ((), copied) = sshub::osc52::capture(|| press(&mut app, 'y'));
    let line = copied.first().expect("nothing was copied").clone();

    // Let a POSIX shell split the line the way a paste would, and print each
    // word on its own line: the words are exactly ssh's argv.
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("printf '%s\\n' {line}"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{line}");
    let words: Vec<String> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    assert_eq!(words.first().map(String::as_str), Some("ssh"), "{line}");
    assert!(words.contains(&"-J".to_string()) && words.contains(&"10.0.0.1".to_string()));
    assert_eq!(
        words.last().unwrap(),
        &format!("o'brien@{address}"),
        "the target is one word: {words:?}"
    );
    assert!(
        !marker.exists(),
        "the pasted line ran a command substitution"
    );
}

/// A profile with hosts but no jump edges (design D07).
fn edgeless_app() -> App {
    let store = Arc::new(LauncherStore::open_in_memory().unwrap());
    for name in ["alpha", "beta", "gamma"] {
        store
            .create_host(&NewHost::launcher(name, format!("{name}.example")))
            .unwrap();
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

#[test]
fn with_no_jump_edges_the_map_shows_the_d07_empty_state_at_155x31() {
    let mut app = edgeless_app();
    to_map(&mut app);
    for (w, h) in [(160, 45), (155, 31)] {
        let text = screen(&draw(&app, w, h));
        for needle in [
            "No jump edges yet.",
            "All 3 hosts are reached directly, so there is no hop to hang them behind.",
            "A row appears when a host, its group, or ~/.ssh/config sets a ProxyJump.",
            "on a host: route other hosts through it",
            "edit a host",
            "give a group a default ProxyJump",
            "Shift+Z",
            "per-group health board",
            "e edit host   G groups   u use as jump   Shift+Z show direct hosts   1 hosts",
        ] {
            assert!(text.contains(needle), "{w}x{h} missing {needle:?}:\n{text}");
        }
    }
}

#[test]
fn the_d07_actions_work_and_shift_z_opens_the_per_group_board() {
    let mut app = edgeless_app();
    to_map(&mut app);
    // `u` with no hop picks one first.
    press(&mut app, 'u');
    assert!(app.map_view.as_ref().unwrap().use_picker.is_some());
    app.handle_key(key(KeyCode::Esc)).unwrap();
    // `G` opens the group manager.
    press(&mut app, 'G');
    assert_ne!(app.mode, AppMode::Normal);
    app.handle_key(key(KeyCode::Esc)).unwrap();
    assert_eq!(app.active_tab, 5);
    // `Shift+Z` gives D14: one glyph strip per group.
    press(&mut app, 'Z');
    let text = screen(&draw(&app, 160, 45));
    assert!(text.contains("direct-only expanded"), "{text}");
    assert!(text.contains("one glyph per host"), "{text}");
    // `e` edits the selected host (or opens a picker).
    press(&mut app, 'e');
    assert!(
        app.mode == AppMode::HostForm || app.map_view.as_ref().unwrap().use_picker.is_some(),
        "mode {:?}",
        app.mode
    );
}

#[test]
fn down_from_a_hop_lands_on_the_hop_drawn_on_the_next_row() {
    let mut app = app();
    to_map(&mut app);
    let mut seen = 0;
    for _ in 0..6 {
        let v = app.map_view.as_ref().unwrap();
        // On a hop the selection is the node of the row under the cursor, and
        // the ribbon names it: never an ancestor the row does not draw.
        if let Some(MapSel::Hop(n)) = v.selection() {
            let row = v.rows[v.row];
            assert_eq!(row, sshub::map::Row::Node(n), "{:?}", v.selection());
            let label = v.map.key_label(&v.map.nodes[n].key);
            let text = screen(&draw(&app, 160, 45));
            assert!(
                text.contains(&format!("path ribbon \u{2500} {label}")),
                "{text}"
            );
            seen += 1;
        }
        app.handle_key(key(KeyCode::Down)).unwrap();
    }
    assert!(seen >= 2, "walked {seen} hop rows");
}
