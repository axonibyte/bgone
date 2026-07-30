#![allow(dead_code)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// RAII wrapper to guarantee test directories and files are cleaned up on completion or panic.
pub struct TempDir {
    pub path: PathBuf,
}

impl TempDir {
    pub fn new(prefix: &str) -> Self {
        let count = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "bgone_test_{}_{}_{}",
            prefix,
            std::process::id(),
            count
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("Failed to create temporary test directory");
        Self { path }
    }

    /// Absolute path of `name` inside this directory.
    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Builds a minimal ports tree containing a single port and returns its root.
pub fn write_mock_ports_tree(temp: &TempDir) -> PathBuf {
    let ports_root = temp.join("ports");
    let nginx_dir = ports_root.join("www").join("nginx");
    fs::create_dir_all(&nginx_dir).unwrap();
    fs::write(
        nginx_dir.join("Makefile"),
        "PORTNAME=   nginx\n\
         PORTVERSION=1.24.0\n\
         COMMENT=    Robust HTTP and reverse proxy server\n\
         OPTIONS_DEFINE=   HTTP2 DOCS\n\
         OPTIONS_DEFAULT=  HTTP2\n",
    )
    .unwrap();
    ports_root
}
