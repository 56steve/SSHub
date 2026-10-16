pub mod fixture_resolver;

pub use fixture_resolver::FixtureResolver;

/// Point `HOME` and the ssh config at an empty scratch dir, once per test
/// binary: tests that spawn `ssh` or open the known-hosts overlay must never
/// read the developer's real `~/.ssh`.
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
