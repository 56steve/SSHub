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

/// Point `HOME` and the ssh config at an empty scratch dir, once per test
/// binary: tests that spawn `ssh` or open the known-hosts overlay must never
/// read the developer's real `~/.ssh`.
///
/// Caveat: this changes process-wide environment, once, from whichever test
/// calls it first, while other tests of the same binary (`first_run`,
/// `import_export`, `local_shell`) set and remove their own variables. That is
/// a race by construction. It is harmless today because every config involved
/// is empty, but a test that depends on `HOME` or `SSHUB_SSH_CONFIG` having a
/// particular value must set it itself instead of relying on this.
#[allow(dead_code)]
pub fn isolate_home() {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ssh")).unwrap();
        std::fs::write(dir.path().join(".ssh/config"), "").unwrap();
        std::env::set_var("HOME", dir.path());
        std::env::set_var("SSHUB_SSH_CONFIG", dir.path().join(".ssh/config"));
        std::env::remove_var("SSH_LAUNCHER_SSH_CONFIG");
        dir
    });
}
