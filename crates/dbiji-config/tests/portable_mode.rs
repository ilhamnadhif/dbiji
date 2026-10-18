//! `DBIJI_HOME` portable mode. This file holds a single test so changing the
//! process environment cannot race with other tests.

use dbiji_config::{AppPaths, settings};

#[test]
fn dbiji_home_overrides_every_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("portable");
    // SAFETY: this test binary runs only this test, so no other thread reads
    // the environment concurrently.
    unsafe {
        std::env::set_var(dbiji_config::paths::ENV_HOME, &root);
    }
    let paths = AppPaths::new().unwrap();
    unsafe {
        std::env::remove_var(dbiji_config::paths::ENV_HOME);
    }
    assert!(paths.is_portable());
    assert_eq!(paths.config_dir(), root.join("config"));
    assert_eq!(paths.data_dir(), root.join("data"));
    assert_eq!(paths.vault_file(), root.join("data").join("vault.bin"));
    assert_eq!(
        paths.known_hosts_file(),
        root.join("data").join("known_hosts")
    );

    paths.ensure_dirs().unwrap();
    let mut s = settings::load(&paths);
    s.editor.font_size = 17.0;
    settings::save(&paths, &s).unwrap();
    assert!(root.join("config").join("settings.json").is_file());
    assert_eq!(settings::load(&paths).editor.font_size, 17.0);
}
