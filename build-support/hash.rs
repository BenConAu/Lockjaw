// Shared build-time hash computation for Lockjaw.
// Included by every crate's build.rs via include!().
// Computes FNV-1a hash of all .rs source files across the project.
// The hash is embedded in every binary and checked at boot to prevent
// stale binary mismatches.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

/// Source directories to hash, relative to the project root.
const SOURCE_DIRS: &[&str] = &[
    "src",
    "lockjaw-types/src",
    "user/init/src",
    "user/hello/src",
    "user/uart-driver/src",
    "user/device-manager/src",
    "user/ramfb-driver/src",
    "user/lockjaw-userlib/src",
];

/// Compute FNV-1a hash of all .rs files under the Lockjaw project.
/// Files are sorted by path for determinism across platforms and runs.
fn compute_source_hash(project_root: &Path) -> u64 {
    let mut files = BTreeSet::new();
    for dir in SOURCE_DIRS {
        let full = project_root.join(dir);
        if full.exists() {
            collect_rs_files(&full, &mut files);
        }
    }

    let mut hash = FNV_OFFSET;
    for path in &files {
        let content = fs::read(path).unwrap_or_else(|e| {
            panic!("build-support/hash.rs: cannot read {}: {}", path.display(), e);
        });
        for &b in &content {
            hash ^= b as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    hash
}

fn collect_rs_files(dir: &Path, out: &mut BTreeSet<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
            out.insert(path);
        }
    }
}

/// Write the source hash to OUT_DIR/source_hash.rs as a u64 const literal.
/// Call from every crate's build.rs main().
fn write_source_hash(project_root: &Path) {
    let hash = compute_source_hash(project_root);
    let out_dir = std::env::var("OUT_DIR").unwrap();
    fs::write(
        format!("{}/source_hash.rs", out_dir),
        format!("0x{:016x}_u64", hash),
    ).unwrap();
}
