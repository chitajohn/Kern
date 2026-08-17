//! Store file permissions (ARCHITECTURE.md §25).
//!
//! The database holds agent configs, tool results, memory, and event
//! history — a copied DB is a data-leak vector. `Store::open` restricts the
//! DB, WAL, and lock files to the owning user where the platform supports
//! it (SQLite otherwise creates files with the umask, commonly group/world
//! readable). This test proves the files are not group/world accessible.

use kern_core::store::Store;

#[cfg(unix)]
#[test]
fn database_files_are_private() {
    use kern_core::store::{DB_FILE, LOCK_FILE};
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    for name in [DB_FILE, LOCK_FILE] {
        let path = dir.path().join(name);
        let mode = std::fs::metadata(&path)
            .unwrap_or_else(|e| panic!("{name} metadata: {e}"))
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "{name} must not be group/world accessible (mode {mode:o})"
        );
    }
    // The WAL file exists while the connection is open; it must be private
    // too (it can hold uncheckpointed pages).
    let wal = dir.path().join(format!("{DB_FILE}-wal"));
    if wal.exists() {
        let mode = std::fs::metadata(&wal).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "WAL must be private (mode {mode:o})");
    }
    drop(store);
}

#[cfg(not(unix))]
#[test]
fn store_opens_on_non_unix() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    drop(store);
}
