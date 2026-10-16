//! Test connect wiring (issue #79): build the steps for a chain, run them on a
//! worker, and fold the events into [`TestRun`] for the map and the hop editor.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use super::network_map::MapSel;
use super::*;
use crate::map::{hops, parse_raw_hop, Chain, HopKey, HopKind};
use crate::ssh::test_connect::{spawn_test, test_runner, StepState, TestEvent, TestStep};

/// Who asked for the run, so only that screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestOwner {
    Map(MapSel),
    Editor,
}

pub struct TestRun {
    pub owner: TestOwner,
    /// What is being tested, for titles.
    pub title: String,
    pub steps: Vec<TestStep>,
    pub states: Vec<StepState>,
    rx: Receiver<TestEvent>,
    cancel: Arc<AtomicBool>,
    pub done: bool,
}

impl TestRun {
    pub fn running(&self) -> bool {
        !self.done
    }

    /// First failed step.
    pub fn failure(&self) -> Option<(usize, &str, &str)> {
        self.states.iter().enumerate().find_map(|(i, s)| match s {
            StepState::Failed { reason, detail } => Some((i, reason.as_str(), detail.as_str())),
            _ => None,
        })
    }

    /// Title line: progress while running, outcome after.
    pub fn headline(&self) -> String {
        let total = self.steps.len();
        if let Some((i, reason, _)) = self.failure() {
            return format!("failed at hop {} of {total} ({reason})", i + 1);
        }
        if self.done {
            if self.states.contains(&StepState::Cancelled) {
                return "cancelled".into();
            }
            return format!("all {total} steps ok");
        }
        let at = self
            .states
            .iter()
            .position(|s| matches!(s, StepState::Running | StepState::Pending))
            .unwrap_or(total.saturating_sub(1));
        format!("step {} of {total}", at + 1)
    }
}

/// The target a chain test ends on.
pub(crate) enum TestTarget {
    /// A host of [`App::hosts`].
    Host(usize),
    /// The host form being edited (not necessarily saved).
    Form,
    /// The last hop is the target: no extra step.
    None,
}

/// One hop of the chain under test: a name/raw token, or a ProxyCommand hop
/// (shown, skipped, and not part of the `-J` prefix).
pub(crate) type TestHop = Option<String>;

const PROXY_SKIP: &str = "proxy cmd, not tested";

impl App {
    /// `t` on the map: test the path of what the cursor is on.
    pub(crate) fn map_start_test(&mut self) {
        if self.test_run.as_ref().is_some_and(TestRun::running) {
            self.host_notice = Some("a test is already running (Esc cancels it)".into());
            return;
        }
        let Some(v) = self.map_view.as_ref() else {
            return;
        };
        let Some(sel) = v.selection() else {
            return;
        };
        let map = &v.map;
        let hop_name = |k: &HopKey| -> TestHop {
            match k {
                HopKey::Saved(h) => Some(map.hosts[*h].name.clone()),
                HopKey::Raw(r) => Some(r.label()),
                HopKey::Invalid(t) => Some(t.clone()),
                HopKey::ProxyCmd(_) => None,
            }
        };
        let (hops, target, title) = match sel {
            MapSel::Hop(n) => {
                let hops: Vec<TestHop> = map
                    .node_path(n)
                    .into_iter()
                    .map(|p| hop_name(&map.nodes[p].key))
                    .collect();
                (hops, TestTarget::None, map.key_label(&map.nodes[n].key))
            }
            MapSel::Host(h) => match &map.chains[h] {
                Chain::Hops(c) => {
                    let hops = c
                        .iter()
                        .map(|hop| match &hop.kind {
                            HopKind::ProxyCommand(_) => None,
                            // The token as written, so `user@` and `:port`
                            // reach ssh exactly as a real connect sends them.
                            _ => Some(hop.token.clone()),
                        })
                        .collect();
                    (hops, TestTarget::Host(h), map.hosts[h].name.clone())
                }
                Chain::Cycle(_) => {
                    self.host_notice = Some("jump loop: nothing to test, fix the ProxyJump".into());
                    return;
                }
            },
            _ => {
                self.host_notice = Some("put the cursor on a hop or host to test its path".into());
                return;
            }
        };
        let owner = TestOwner::Map(sel);
        self.start_test(owner, title, &hops, target);
    }

    /// `t` in the hop editor: test the chain as it is in the form, ending on
    /// the host being edited.
    pub(crate) fn editor_start_test(&mut self) {
        if self.test_run.as_ref().is_some_and(TestRun::running) {
            return;
        }
        let Some(form) = self.host_form.as_ref() else {
            return;
        };
        let own = form.proxy_jump.trim();
        let tokens = if own.is_empty() {
            // The inherited chain is what a connect would use.
            let primary = self
                .groups
                .iter()
                .find(|g| form.group_ids.contains(&g.id))
                .map(|g| g.id);
            let selected: Vec<i64> = form.group_ids.iter().copied().collect();
            self.store
                .inherited_for_groups(primary, &selected)
                .ok()
                .and_then(|r| r.proxy_jump)
                .map(|j| hops::split(&j))
                .unwrap_or_default()
        } else {
            hops::split(own)
        };
        let title = if form.name.trim().is_empty() {
            "this host".to_string()
        } else {
            form.name.trim().to_string()
        };
        let hops: Vec<TestHop> = tokens.into_iter().map(Some).collect();
        self.start_test(TestOwner::Editor, title, &hops, TestTarget::Form);
    }

    fn start_test(
        &mut self,
        owner: TestOwner,
        title: String,
        hops: &[TestHop],
        target: TestTarget,
    ) {
        let mut steps: Vec<TestStep> = Vec::new();
        for i in 0..hops.len() {
            steps.push(self.hop_test_step(hops, i));
        }
        match target {
            TestTarget::None => {}
            TestTarget::Host(h) => {
                let prefix = self.test_prefix(hops, hops.len());
                steps.push(self.entry_test_step(h, prefix));
            }
            TestTarget::Form => {
                let prefix = self.test_prefix(hops, hops.len());
                steps.push(self.form_test_step(prefix));
            }
        }
        if steps.is_empty() {
            self.host_notice = Some("nothing to test".into());
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let rx = spawn_test(steps.clone(), Arc::new(test_runner()), Arc::clone(&cancel));
        self.test_run = Some(TestRun {
            owner,
            title,
            states: vec![StepState::Pending; steps.len()],
            steps,
            rx,
            cancel,
            done: false,
        });
    }

    /// Esc: stop a running test, or clear a finished one. `true` when it did.
    pub(crate) fn cancel_or_clear_test(&mut self, owner_is_editor: bool) -> bool {
        let Some(run) = self.test_run.as_mut() else {
            return false;
        };
        if (run.owner == TestOwner::Editor) != owner_is_editor {
            return false;
        }
        if run.running() {
            run.cancel.store(true, Ordering::Relaxed);
        } else {
            self.test_run = None;
        }
        true
    }

    /// Fold worker events into the run; called every tick.
    pub fn poll_test_run(&mut self) {
        let Some(run) = self.test_run.as_mut() else {
            return;
        };
        let mut log: Vec<crate::ssh::probe::SshLogEntry> = Vec::new();
        while let Ok(ev) = run.rx.try_recv() {
            match ev {
                TestEvent::Step { index, state } => {
                    // A failed step leaves its command and reason in the ssh
                    // log of that host, where `l` on the map shows it.
                    if let (StepState::Failed { reason, detail }, Some(step)) =
                        (&state, run.steps.get(index))
                    {
                        let host = step.host.clone().unwrap_or_else(|| step.label.clone());
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_secs() as i64);
                        let entry = |line: String, level| crate::ssh::probe::SshLogEntry {
                            host_name: host.clone(),
                            line,
                            level,
                            timestamp: now,
                        };
                        log.push(entry(
                            format!("$ {}", step.cmd),
                            crate::ssh::probe::LogLevel::Info,
                        ));
                        log.push(entry(
                            format!("test connect: {reason} {detail}")
                                .trim_end()
                                .to_string(),
                            crate::ssh::probe::LogLevel::Error,
                        ));
                    }
                    if let Some(slot) = run.states.get_mut(index) {
                        *slot = state;
                    }
                }
                TestEvent::Done => run.done = true,
            }
        }
        for e in log {
            self.push_ssh_log(e);
        }
    }

    /// `-J` string for the first `n` hops (ProxyCommand hops are not in it),
    /// expanded the way connect expands it. `None` when there is none.
    fn test_prefix(&self, hops: &[TestHop], n: usize) -> Result<Option<String>, String> {
        let names: Vec<String> = hops[..n].iter().flatten().cloned().collect();
        // ssh would run that hop's ProxyCommand to get through it: not tested.
        if names.iter().any(|t| self.token_has_proxy_command(t)) {
            return Err(PROXY_SKIP.to_string());
        }
        if names.is_empty() {
            return Ok(None);
        }
        self.store
            .expand_jump(&hops::join(&names))
            .map(Some)
            .map_err(|e| format!("{e:#}"))
    }

    /// The saved host a hop token names (`name`, `user@name`, `name:port`).
    fn token_host(&self, token: &str) -> Option<usize> {
        let by_name = |n: &str| self.hosts.iter().position(|h| h.name() == n);
        by_name(token).or_else(|| {
            parse_raw_hop(token)
                .filter(|r| !r.ipv6)
                .and_then(|r| by_name(&r.host))
        })
    }

    fn token_has_proxy_command(&self, token: &str) -> bool {
        self.token_host(token)
            .is_some_and(|i| entry_has_proxy_command(&self.hosts[i]))
    }

    fn hop_test_step(&self, hops: &[TestHop], i: usize) -> TestStep {
        let Some(token) = hops[i].as_deref() else {
            return skipped("proxy cmd", PROXY_SKIP);
        };
        let prefix = match self.test_prefix(hops, i) {
            Ok(p) => p,
            Err(e) => return skipped(token, &e),
        };
        if let Some(idx) = self.token_host(token) {
            return self.entry_test_step(idx, Ok(prefix));
        }
        match parse_raw_hop(token) {
            Some(raw) => {
                let mut host = SshHost::new(raw.host.clone());
                host.hostname = Some(raw.host);
                host.user = raw.user;
                host.port = raw.port;
                host.proxy_jump = prefix;
                finish(token, None, crate::ssh::build_ssh_argv(&host), None)
            }
            None => skipped(token, "invalid hop, not tested"),
        }
    }

    /// Step for a saved host reached through `prefix`.
    fn entry_test_step(&self, idx: usize, prefix: Result<Option<String>, String>) -> TestStep {
        let entry = &self.hosts[idx];
        let name = entry.name().to_string();
        let prefix = match prefix {
            Ok(p) => p,
            Err(e) => return skipped(&name, &e),
        };
        if entry_has_proxy_command(entry) {
            return skipped(&name, PROXY_SKIP);
        }
        let alias = |prefix: &Option<String>| {
            let mut argv = vec!["ssh".to_string()];
            if let Some(j) = prefix {
                argv.push("-J".into());
                argv.push(j.clone());
            }
            argv.push(crate::ssh::safe_ssh_target(name.clone()));
            argv
        };
        match entry {
            HostEntry::Legacy { .. } => finish(&name, Some(name.clone()), alias(&prefix), None),
            HostEntry::Managed(m) => {
                let resolved = self
                    .store
                    .resolve_connection(m)
                    .unwrap_or_else(|_| crate::store::ResolvedConnection::from_stored(m));
                let secret = resolve_pending_secret(
                    entry,
                    resolved.identity.as_ref(),
                    self.password_store.as_ref(),
                )
                .0;
                let argv = if m.source == HostSource::SshConfig {
                    alias(&prefix)
                } else {
                    let mut host = resolved_to_ssh_host(m, &resolved);
                    host.proxy_jump = prefix;
                    host.remote_command = None;
                    crate::ssh::build_ssh_argv(&host)
                };
                finish(&name, Some(name.clone()), argv, secret)
            }
        }
    }

    /// The last step of an editor test: the host as the form has it.
    fn form_test_step(&self, prefix: Result<Option<String>, String>) -> TestStep {
        let Some(form) = self.host_form.as_ref() else {
            return skipped("this host", "no form");
        };
        let name = if form.name.trim().is_empty() {
            "this host".to_string()
        } else {
            form.name.trim().to_string()
        };
        let prefix = match prefix {
            Ok(p) => p,
            Err(e) => return skipped(&name, &e),
        };
        let identity = form
            .identity_index
            .checked_sub(1)
            .and_then(|i| self.identities.get(i));
        let mut host = SshHost::new(name.clone());
        host.hostname = Some(form.address.trim().to_string());
        host.user =
            optional_field(&form.username).or_else(|| identity.and_then(|i| i.username.clone()));
        host.port = form.port.trim().parse().ok();
        host.identity_file = identity
            .and_then(|i| i.private_key.as_ref())
            .map(|p| p.to_string_lossy().into_owned());
        host.proxy_jump = prefix;
        if host.hostname.as_deref().unwrap_or("").is_empty() {
            return skipped(&name, "no address yet");
        }
        finish(&name, None, crate::ssh::build_ssh_argv(&host), None)
    }
}

/// The host's ssh_config sets a ProxyCommand (presence only).
fn entry_has_proxy_command(entry: &HostEntry) -> bool {
    match entry {
        HostEntry::Legacy { host, .. } => host.has_proxy_command,
        HostEntry::Managed(m) => m
            .ssh_config_hash
            .as_deref()
            .is_some_and(crate::ssh::ssh_config_hash_has_proxy_command),
    }
}

fn skipped(label: &str, reason: &str) -> TestStep {
    TestStep {
        label: label.to_string(),
        host: None,
        argv: Vec::new(),
        secret: None,
        cmd: String::new(),
        skip: Some(reason.to_string()),
    }
}

fn finish(
    label: &str,
    host: Option<String>,
    argv: Vec<String>,
    secret: Option<crate::session::PendingSecret>,
) -> TestStep {
    TestStep {
        label: label.to_string(),
        host,
        cmd: format!("{} true", argv.join(" ")),
        argv,
        secret,
        skip: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{host, test_app};
    use crate::store::NewHost;

    #[test]
    fn a_hop_with_proxy_command_stops_every_step_that_goes_through_it() {
        let mut edge = host("edge");
        edge.has_proxy_command = true;
        let mut app = test_app(vec![("edge", edge), ("tail", host("tail"))]);
        app.reload_hosts().unwrap();
        let hops: Vec<TestHop> = vec![Some("edge".into()), Some("tail".into())];

        // The prefix of any step after `edge` must not become `ssh -J edge`.
        assert_eq!(app.test_prefix(&hops, 1), Err(PROXY_SKIP.to_string()));
        let steps = [
            app.hop_test_step(&hops, 0),
            app.hop_test_step(&hops, 1),
            app.entry_test_step(1, app.test_prefix(&hops, 2)),
        ];
        for (i, step) in steps.iter().enumerate() {
            assert_eq!(step.skip.as_deref(), Some(PROXY_SKIP), "step {i}");
            assert!(step.argv.is_empty(), "step {i} must not run ssh");
        }
    }

    #[test]
    fn hop_tokens_keep_their_user_and_port_in_the_prefix() {
        let mut app = test_app(vec![]);
        let mut j = NewHost::launcher("jump", "10.0.0.2");
        j.username = Some("ops".into());
        app.store().create_host(&j).unwrap();
        app.reload_hosts().unwrap();
        let hops: Vec<TestHop> = vec![Some("root@jump:2200".into()), Some("x".into())];
        // What a real connect sends: the override wins over the stored values.
        assert_eq!(
            app.test_prefix(&hops, 1),
            Ok(Some("root@10.0.0.2:2200".to_string()))
        );
        // And the step for that hop finds the saved host behind the token.
        assert!(app.token_host("root@jump:2200").is_some());
    }
}
