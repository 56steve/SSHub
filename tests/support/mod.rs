pub mod fixture_resolver;

pub use fixture_resolver::FixtureResolver;

/// A tool an oracle test needs is missing: skip, or fail when `CI` is set, so a
/// runner without it cannot turn the oracles into silent no-ops.
#[allow(dead_code)]
pub fn tool_missing(what: &str) -> bool {
    assert!(
        std::env::var_os("CI").is_none(),
        "{what} is missing on a CI runner: the oracle tests would silently skip"
    );
    eprintln!("skipping: no {what}");
    true
}

/// `ssh` runs (with the isolated home of [`isolate_home`]); otherwise skip, or
/// fail on CI.
#[allow(dead_code)]
pub fn have_ssh() -> bool {
    isolate_home();
    std::process::Command::new("ssh").arg("-V").output().is_ok() || {
        tool_missing("ssh");
        false
    }
}

/// The one lock every environment-mutating e2e test (and [`isolate_home`]) takes,
/// so no two of them are ever mid-mutation at once. Not reentrant: never call
/// [`isolate_home`]/[`have_ssh`] while holding an [`EnvGuard`].
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Holds the env lock and puts every variable it changed back on drop, so a
/// test's overrides neither leak into nor race with another test's.
#[allow(dead_code)]
pub struct EnvGuard {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[allow(dead_code)]
impl EnvGuard {
    pub fn new() -> Self {
        Self {
            saved: Vec::new(),
            _lock: env_lock(),
        }
    }

    pub fn set(&mut self, key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        self.saved.push((key, std::env::var_os(key)));
        std::env::set_var(key, value);
        self
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, previous) in self.saved.drain(..).rev() {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// Point `HOME` and the ssh config at an empty scratch dir, once per test
/// binary: tests that spawn `ssh` or open the known-hosts overlay must never
/// read the developer's real `~/.ssh`.
///
/// The one-time mutation runs under the same lock as [`EnvGuard`], so it cannot
/// interleave with a test that holds overrides. Because `HOME` then stays set
/// for the rest of the binary, a test that depends on `HOME` or
/// `SSHUB_SSH_CONFIG` having a particular value must set it via [`EnvGuard`].
#[allow(dead_code)]
pub fn isolate_home() {
    static HOME: std::sync::LazyLock<tempfile::TempDir> = std::sync::LazyLock::new(|| {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ssh")).unwrap();
        std::fs::write(dir.path().join(".ssh/config"), "").unwrap();
        std::env::set_var("HOME", dir.path());
        std::env::set_var("SSHUB_SSH_CONFIG", dir.path().join(".ssh/config"));
        std::env::remove_var("SSH_LAUNCHER_SSH_CONFIG");
        dir
    });
    std::sync::LazyLock::force(&HOME);
}

/// A loopback port nothing listens on right now, never handed out twice by this
/// process: the bind-and-drop probe alone let two parallel tests (or two
/// calls in one test) receive the same port.
#[allow(dead_code)]
pub fn free_port() -> u16 {
    static HANDED_OUT: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
    let mut handed = HANDED_OUT.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        if !handed.contains(&port) {
            handed.push(port);
            return port;
        }
    }
}
