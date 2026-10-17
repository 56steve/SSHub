//! Test connect for a jump chain (issue #79): one `ssh ... true` per hop, each
//! through the hops before it, stopping at the first failure.
//!
//! Step *k* is `ssh -o BatchMode=... -J h1,...,h(k-1) hk true`. Nothing here
//! writes `known_hosts` (no `accept-new`): an unknown host key is reported as
//! `host key unknown`, not trusted. A step whose host is reached through a
//! ProxyCommand is skipped, since running one is the user's ssh_config's
//! business and the diagnostic must not execute it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::broadcast::{CommandRunner, SshCommandRunner};

/// Per-step connect timeout and whole-step cap.
pub const STEP_CONNECT_TIMEOUT_SECS: u32 = 10;
pub const STEP_RUN_TIMEOUT: Duration = Duration::from_secs(30);

/// A stored secret to fetch at the moment a step runs.
pub type SecretSource = Arc<dyn Fn() -> Option<crate::session::PendingSecret> + Send + Sync>;

/// The jump hosts' own stored secrets for a step, fetched when it runs.
pub type HopsSource = Arc<dyn Fn() -> Vec<crate::session::hop_auth::HopSecret> + Send + Sync>;

/// What to run for one hop.
#[derive(Clone)]
pub struct TestStep {
    /// The hop's display name.
    pub label: String,
    /// The saved host this step is about, when it is one.
    pub host: Option<String>,
    /// `argv[0] == "ssh"`, ending in the target; `true` is appended.
    pub argv: Vec<String>,
    /// Looks the stored secret up when the step is about to run, so nothing
    /// touches the keyring before then (or at all, for `y`) and no secret sits
    /// in the run afterwards.
    pub secret: Option<SecretSource>,
    /// The hops before this step, each with its own stored secret (never the
    /// step's own): a hop that asks for a password is answered, not left to fail.
    pub hops: Option<HopsSource>,
    /// The command as shown to the user, e.g. `ssh -J a b true`.
    pub cmd: String,
    /// The short form for a panel row: hop names as written, not addresses.
    pub shown: String,
    /// Do not run: why.
    pub skip: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Running,
    Ok {
        ms: u64,
    },
    /// `reason` is one of the short words of [`classify_failure`].
    Failed {
        reason: String,
        detail: String,
    },
    Skipped {
        reason: String,
    },
    Cancelled,
}

impl StepState {
    pub fn is_failure(&self) -> bool {
        matches!(self, StepState::Failed { .. })
    }
}

#[derive(Debug, Clone)]
pub enum TestEvent {
    Step { index: usize, state: StepState },
    Done,
}

/// Short reason and detail for a failed `ssh` run: `stderr` is what ssh printed,
/// `exit` its status. Matches the strings OpenSSH prints (checked against a real
/// client and `sshd` in the oracle tests below).
pub fn classify_failure(exit: i32, stderr: &str) -> (String, String) {
    let lower = stderr.to_lowercase();
    let last = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string();
    let reason = if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        // `Offending ED25519 key in /home/u/.ssh/known_hosts:42`: the file and
        // line to fix.
        let at = stderr
            .lines()
            .find_map(|l| l.trim().strip_prefix("Offending "))
            .and_then(|l| l.split_once(" key in "))
            .and_then(|(_, loc)| loc.rsplit_once(':'))
            .map(|(file, n)| {
                let name = std::path::Path::new(file)
                    .file_name()
                    .map_or(file.to_string(), |f| f.to_string_lossy().into_owned());
                format!("{name}:{}", n.trim())
            });
        return (
            "host key changed".into(),
            at.unwrap_or_else(|| last.clone()),
        );
    } else if lower.contains("permission denied (") {
        // The server's verdict: `Permission denied (publickey,password).`
        "auth failed"
    } else if lower.contains("load key") && lower.contains("permission denied") {
        // `Load key "k": Permission denied` is a local file problem, not a login.
        "key unreadable"
    } else if lower.contains("permission denied") {
        "auth failed"
    } else if lower.contains("host key verification failed") {
        "host key unknown"
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "timeout"
    } else if lower.contains("connection refused") {
        "refused"
    } else if lower.contains("could not resolve hostname")
        || lower.contains("name or service not known")
        || lower.contains("nodename nor servname")
    {
        "unknown host"
    } else if lower.contains("proxycommand") || lower.contains("unknown port 65535") {
        "proxy cmd failed"
    } else {
        return (format!("failed (exit {exit})"), last);
    };
    (reason.into(), last)
}

/// The runner test connect uses: key/agent only unless the step has a stored
/// secret, no `accept-new`, and the whole process group dies on cancel.
pub fn test_runner() -> SshCommandRunner {
    SshCommandRunner {
        connect_timeout_secs: STEP_CONNECT_TIMEOUT_SECS,
        run_timeout: STEP_RUN_TIMEOUT,
        accept_new_host_keys: false,
        kill_process_group: true,
        isolated_config: Some(
            crate::ssh::ssh_config_path()
                .unwrap_or_else(|_| crate::ssh::expand_tilde("~/.ssh/config")),
        ),
    }
}

/// The `-F` file of a test step: first-match-wins, so these come before the
/// user's own config, which is `Include`d after them. `ProxyCommand none` is
/// what keeps a wildcard `ProxyCommand` of the user's from running in the `-W`
/// helper of a `-J` hop. With a stored secret the hops may prompt (askpass
/// answers them); without one nothing may.
pub fn isolated_config(user_config: &std::path::Path, askpass: bool) -> String {
    let mut text = format!(
        "Host *\n    ProxyCommand none\n    BatchMode {}\n    StrictHostKeyChecking yes\n    UpdateHostKeys no\n",
        if askpass { "no" } else { "yes" }
    );
    let path = user_config.to_string_lossy();
    // No way to quote these in ssh_config: leave the user's config out.
    if !path.contains(['"', '\n', '\r']) {
        text.push_str(&format!("Include \"{path}\"\n"));
    }
    text
}

/// Run `steps` in order on a worker thread; `cancel` stops it and kills the
/// running ssh (and its `-W` helper). Events arrive on the returned channel;
/// `Done` always comes last.
pub fn spawn_test(
    steps: Vec<TestStep>,
    runner: Arc<dyn CommandRunner>,
    cancel: Arc<AtomicBool>,
) -> Receiver<TestEvent> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let send = |index: usize, state: StepState| {
            let _ = tx.send(TestEvent::Step { index, state });
        };
        let mut stopped = false;
        for (i, step) in steps.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                send(i, StepState::Cancelled);
                stopped = true;
                continue;
            }
            // After a failure the rest was not tried: skipped, not cancelled.
            if stopped {
                send(
                    i,
                    StepState::Skipped {
                        reason: "skipped".into(),
                    },
                );
                continue;
            }
            if let Some(reason) = &step.skip {
                send(
                    i,
                    StepState::Skipped {
                        reason: reason.clone(),
                    },
                );
                continue;
            }
            send(i, StepState::Running);
            let started = Instant::now();
            let secret = step.secret.as_ref().and_then(|fetch| fetch());
            let hops = step.hops.as_ref().map(|fetch| fetch()).unwrap_or_default();
            let result = runner.run_with_hops(&step.argv, "true", secret.as_ref(), &hops, &cancel);
            drop((secret, hops));
            let state = match result {
                // 255 is ssh's own failure. Any other non-zero status is the
                // remote command's: the login worked, but a `nologin` /
                // `ForceCommand` / restricted bastion did not run `true`, and
                // such a host still forwards for the next hop.
                Ok((exit, _, _)) if exit == 0 || (0..255).contains(&exit) => StepState::Ok {
                    ms: started.elapsed().as_millis() as u64,
                },
                Ok((exit, _, stderr)) => {
                    let (reason, detail) = classify_failure(exit, &stderr);
                    StepState::Failed { reason, detail }
                }
                Err(e) if e == "cancelled" => StepState::Cancelled,
                Err(e) if e == "timed out" => StepState::Failed {
                    reason: "timeout".into(),
                    detail: format!("no answer in {}s", STEP_RUN_TIMEOUT.as_secs()),
                },
                Err(e) => StepState::Failed {
                    reason: "failed".into(),
                    detail: e,
                },
            };
            if matches!(state, StepState::Failed { .. } | StepState::Cancelled) {
                stopped = true;
            }
            send(i, state);
        }
        let _ = tx.send(TestEvent::Done);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::tempdir_sshd;
    use std::process::Command;

    fn have_ssh() -> bool {
        Command::new("ssh").arg("-V").output().is_ok() || {
            crate::test_support::tool_missing("ssh");
            false
        }
    }

    /// `ssh -F /dev/null` args for the tempdir sshd with the given key and
    /// known_hosts, as `build_ssh_argv` + the connect path would produce.
    fn argv(
        sshd: &crate::test_support::TempdirSshd,
        key: &std::path::Path,
        known: &std::path::Path,
    ) -> Vec<String> {
        let user = crate::test_support::current_user();
        [
            "ssh",
            "-F",
            "/dev/null",
            "-i",
            key.to_str().unwrap(),
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            &format!("UserKnownHostsFile={}", known.display()),
            "-p",
            &sshd.port.to_string(),
            &format!("{user}@127.0.0.1"),
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    fn known_hosts(sshd: &crate::test_support::TempdirSshd, path: &std::path::Path) {
        let pubkey = std::fs::read_to_string(sshd.host_key.with_extension("pub")).unwrap();
        let mut parts = pubkey.split_whitespace();
        let (kind, blob) = (parts.next().unwrap(), parts.next().unwrap());
        std::fs::write(path, format!("[127.0.0.1]:{} {kind} {blob}\n", sshd.port)).unwrap();
    }

    fn run(argv: Vec<String>) -> StepState {
        let step = TestStep {
            label: "t".into(),
            host: None,
            argv,
            secret: None,
            hops: None,
            cmd: String::new(),
            shown: String::new(),
            skip: None,
        };
        let runner = Arc::new(SshCommandRunner {
            connect_timeout_secs: 2,
            run_timeout: Duration::from_secs(15),
            accept_new_host_keys: false,
            kill_process_group: true,
            isolated_config: None,
        });
        let rx = spawn_test(vec![step], runner, Arc::new(AtomicBool::new(false)));
        let mut last = StepState::Pending;
        while let Ok(ev) = rx.recv() {
            match ev {
                TestEvent::Step { state, .. } => last = state,
                TestEvent::Done => break,
            }
        }
        last
    }

    #[test]
    fn oracle_ok_against_a_real_sshd() {
        let Some(sshd) = tempdir_sshd() else {
            return;
        };
        let known = sshd.dir.path().join("kh");
        known_hosts(&sshd, &known);
        let state = run(argv(&sshd, &sshd.user_key, &known));
        assert!(matches!(state, StepState::Ok { .. }), "{state:?}");
    }

    #[test]
    fn oracle_auth_failed_unknown_and_changed_host_key_against_a_real_sshd() {
        let Some(sshd) = tempdir_sshd() else {
            return;
        };
        let known = sshd.dir.path().join("kh");
        known_hosts(&sshd, &known);

        // A key the server does not know: Permission denied.
        let other = sshd.dir.path().join("other");
        assert!(Command::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-N",
                "",
                "-q",
                "-f",
                other.to_str().unwrap()
            ])
            .status()
            .unwrap()
            .success());
        match run(argv(&sshd, &other, &known)) {
            StepState::Failed { reason, .. } => assert_eq!(reason, "auth failed"),
            s => panic!("{s:?}"),
        }

        // Empty known_hosts: the key is unknown, and we must not trust it.
        let empty = sshd.dir.path().join("empty");
        std::fs::write(&empty, "").unwrap();
        match run(argv(&sshd, &sshd.user_key, &empty)) {
            StepState::Failed { reason, .. } => assert_eq!(reason, "host key unknown"),
            s => panic!("{s:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&empty).unwrap(),
            "",
            "known_hosts untouched"
        );

        // A different key for this host: REMOTE HOST IDENTIFICATION HAS CHANGED.
        let wrong = sshd.dir.path().join("wrong");
        let blob = std::fs::read_to_string(other.with_extension("pub")).unwrap();
        let mut parts = blob.split_whitespace();
        let (kind, b64) = (parts.next().unwrap(), parts.next().unwrap());
        std::fs::write(&wrong, format!("[127.0.0.1]:{} {kind} {b64}\n", sshd.port)).unwrap();
        match run(argv(&sshd, &sshd.user_key, &wrong)) {
            StepState::Failed { reason, detail } => {
                assert_eq!(reason, "host key changed");
                assert_eq!(detail, "wrong:1");
            }
            s => panic!("{s:?}"),
        }
    }

    #[test]
    fn a_user_config_that_trusts_new_keys_cannot_make_the_test_write_known_hosts() {
        let Some(sshd) = tempdir_sshd() else {
            return;
        };
        // The user's own config asks for exactly what a diagnostic must not do.
        let cfg = sshd.dir.path().join("trusting");
        std::fs::write(
            &cfg,
            "Host *\n  StrictHostKeyChecking accept-new\n  UpdateHostKeys yes\n",
        )
        .unwrap();
        let known = sshd.dir.path().join("kh-empty");
        std::fs::write(&known, "").unwrap();
        let mut a = argv(&sshd, &sshd.user_key, &known);
        a[2] = cfg.to_str().unwrap().to_string(); // replaces `-F /dev/null`
        match run(a) {
            StepState::Failed { reason, .. } => assert_eq!(reason, "host key unknown"),
            s => panic!("{s:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&known).unwrap(),
            "",
            "known_hosts untouched"
        );
    }

    #[test]
    fn oracle_refused_unknown_host_and_timeout_against_real_ssh() {
        if !have_ssh() {
            return;
        }
        let base = |target: &str, port: &str| -> Vec<String> {
            ["ssh", "-F", "/dev/null", "-p", port, target]
                .into_iter()
                .map(String::from)
                .collect()
        };
        // A port nobody listens on.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        match run(base("u@127.0.0.1", &closed.to_string())) {
            StepState::Failed { reason, .. } => assert_eq!(reason, "refused"),
            s => panic!("{s:?}"),
        }
        match run(base("u@no-such-host.invalid", "22")) {
            StepState::Failed { reason, .. } => assert_eq!(reason, "unknown host"),
            s => panic!("{s:?}"),
        }
        // A socket that accepts and never speaks: ssh gives up in the banner
        // exchange after ConnectTimeout.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = silent.local_addr().unwrap().port();
        match run(base("u@127.0.0.1", &port.to_string())) {
            StepState::Failed { reason, .. } => assert_eq!(reason, "timeout"),
            s => panic!("{s:?}"),
        }
    }

    #[test]
    fn remote_command_exit_is_a_working_hop_and_unreadable_key_is_not_auth_failed() {
        struct Fake;
        impl CommandRunner for Fake {
            fn run(
                &self,
                _: &[String],
                _: &str,
                _: Option<&crate::session::PendingSecret>,
                _: &AtomicBool,
            ) -> Result<(i32, String, String), String> {
                // `nologin` / `ForceCommand` bastion: ssh itself succeeded.
                Ok((1, String::new(), "This account is not available.".into()))
            }
        }
        let step = TestStep {
            label: "h".into(),
            host: None,
            argv: vec!["ssh".into()],
            secret: None,
            hops: None,
            cmd: String::new(),
            shown: String::new(),
            skip: None,
        };
        let rx = spawn_test(vec![step], Arc::new(Fake), Arc::new(AtomicBool::new(false)));
        let mut last = StepState::Pending;
        while let Ok(TestEvent::Step { state, .. }) = rx.recv() {
            last = state;
        }
        assert!(matches!(last, StepState::Ok { .. }), "{last:?}");

        assert_eq!(
            classify_failure(255, "Load key \"/k\": Permission denied").0,
            "key unreadable"
        );
        assert_eq!(
            classify_failure(
                255,
                "Load key \"/k\": Permission denied\nu@h: Permission denied (publickey)."
            )
            .0,
            "auth failed"
        );
    }

    #[test]
    fn steps_stop_at_the_first_failure_skip_proxy_cmd_and_cancel_stops_the_rest() {
        struct Fake(
            Vec<Result<(i32, String, String), String>>,
            std::sync::Mutex<usize>,
        );
        impl CommandRunner for Fake {
            fn run(
                &self,
                _: &[String],
                _: &str,
                _: Option<&crate::session::PendingSecret>,
                _: &AtomicBool,
            ) -> Result<(i32, String, String), String> {
                let mut i = self.1.lock().unwrap();
                *i += 1;
                self.0[*i - 1].clone()
            }
        }
        let step = |skip: Option<&str>| TestStep {
            label: "h".into(),
            host: None,
            argv: vec!["ssh".into()],
            secret: None,
            hops: None,
            cmd: String::new(),
            shown: String::new(),
            skip: skip.map(String::from),
        };
        let runner = Arc::new(Fake(
            vec![
                Ok((0, String::new(), String::new())),
                Ok((255, String::new(), "Permission denied (publickey).".into())),
            ],
            Default::default(),
        ));
        let rx = spawn_test(
            vec![
                step(None),
                step(Some("proxy cmd, not tested")),
                step(None),
                step(None),
            ],
            runner,
            Arc::new(AtomicBool::new(false)),
        );
        let mut states = vec![StepState::Pending; 4];
        while let Ok(ev) = rx.recv() {
            match ev {
                TestEvent::Step { index, state } => states[index] = state,
                TestEvent::Done => break,
            }
        }
        assert!(matches!(states[0], StepState::Ok { .. }));
        assert_eq!(
            states[1],
            StepState::Skipped {
                reason: "proxy cmd, not tested".into()
            }
        );
        assert!(matches!(&states[2], StepState::Failed { reason, .. } if reason == "auth failed"));
        assert_eq!(
            states[3],
            StepState::Skipped {
                reason: "skipped".into()
            },
            "nothing runs after the first failure, and it was skipped, not cancelled"
        );
    }

    #[test]
    fn oracle_a_failing_proxy_command_is_classified_from_real_ssh_output() {
        if !have_ssh() {
            return;
        }
        let out = Command::new("ssh")
            .args([
                "-F",
                "/dev/null",
                "-o",
                "ProxyCommand=false",
                "-o",
                "BatchMode=yes",
            ])
            .args(["u@h.invalid", "true"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        let exit = out.status.code().unwrap_or(-1);
        assert_eq!(
            classify_failure(exit, &stderr).0,
            "proxy cmd failed",
            "{stderr}"
        );
        assert_eq!(classify_failure(1, "weird").0, "failed (exit 1)");
    }

    /// Run `ssh [-F user] -J u@127.0.0.1:1 t@127.0.0.1` the way a test step does
    /// (nothing listens on port 1) and say whether the user config's wildcard
    /// `ProxyCommand` ran.
    fn wildcard_proxy_command_ran(isolated: bool) -> bool {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let user = dir.path().join("user.conf");
        std::fs::write(
            &user,
            format!("Host *\n    ProxyCommand touch {}\n", marker.display()),
        )
        .unwrap();
        let mut argv = vec!["ssh".to_string()];
        if !isolated {
            argv.extend(["-F".into(), user.to_string_lossy().into_owned()]);
        }
        argv.extend(["-J", "u@127.0.0.1:1", "t@127.0.0.1"].map(String::from));
        let runner = SshCommandRunner {
            connect_timeout_secs: 2,
            run_timeout: Duration::from_secs(15),
            accept_new_host_keys: false,
            kill_process_group: true,
            isolated_config: isolated.then(|| user.clone()),
        };
        let cancel = AtomicBool::new(false);
        let _ = runner.run(&argv, "true", None, &cancel);
        marker.exists()
    }

    #[test]
    fn oracle_a_wildcard_proxy_command_never_runs_through_a_jump_hop() {
        if !have_ssh() {
            return;
        }
        // The control: without the generated -F, the -W helper of the hop runs it.
        assert!(wildcard_proxy_command_ran(false), "the oracle is blind");
        assert!(
            !wildcard_proxy_command_ran(true),
            "a test step ran the user's ProxyCommand"
        );
    }

    #[test]
    fn a_config_path_that_cannot_be_quoted_is_left_out_of_the_include() {
        assert!(!isolated_config(std::path::Path::new("/a\"b"), false).contains("Include"));
        assert!(isolated_config(std::path::Path::new("/h"), true).contains("BatchMode no"));
    }
}
