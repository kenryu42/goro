//! Goro's own persistent state, outside the repository: recently opened repositories and,
//! per repository, what the user has seen and marked reviewed.
//!
//! Lives in the platform data dir (`GORO_DATA_DIR` overrides it; tests always set it).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How many recent repositories to remember.
const RECENT_LIMIT: usize = 50;

/// FNV-1a: a hash that is stable across runs and Rust versions, for persisted keys.
pub fn stable_hash(parts: &[&[u8]]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for &byte in *part {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        // Separate parts so ["ab", "c"] and ["a", "bc"] differ.
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Per-repository state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoState {
    /// Changed-line hashes per path at the last look; `None` until the first look.
    pub seen: Option<HashMap<String, HashSet<u64>>>,
    /// Hashes of hunks marked reviewed.
    pub reviewed: HashSet<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentRepo {
    pub root: PathBuf,
    /// Seconds since the Unix epoch.
    pub opened_at: u64,
}

#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// `GORO_DATA_DIR`, else the platform's local data dir.
    pub fn open_default() -> Option<Self> {
        let dir = std::env::var_os("GORO_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::data_local_dir().map(|d| d.join("Goro")))?;
        Some(Self { dir })
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn repo_file(&self, root: &Path) -> PathBuf {
        let key = stable_hash(&[root.as_os_str().as_encoded_bytes()]);
        self.dir.join("repos").join(format!("{key:016x}.json"))
    }

    /// Stored state for `root`, or the default if there is none (or it can't be read).
    pub fn repo_state(&self, root: &Path) -> RepoState {
        read_json(&self.repo_file(root)).unwrap_or_default()
    }

    pub fn save_repo_state(&self, root: &Path, state: &RepoState) -> std::io::Result<()> {
        write_json(&self.repo_file(root), state)
    }

    /// Recently opened repositories, most recent first.
    pub fn recent(&self) -> Vec<RecentRepo> {
        read_json(&self.dir.join("recent.json")).unwrap_or_default()
    }

    pub fn touch_recent(&self, root: &Path) -> std::io::Result<()> {
        let mut recent = self.recent();
        recent.retain(|r| r.root != root);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        recent.insert(
            0,
            RecentRepo {
                root: root.to_path_buf(),
                opened_at: now,
            },
        );
        recent.truncate(RECENT_LIMIT);
        write_json(&self.dir.join("recent.json"), &recent)
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write atomically: a crash never leaves a truncated file behind.
fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let dir = path.parent().expect("state files live in a directory");
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, serde_json::to_vec(value)?)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `stable_hash(&[b"a"])`, pinned: persisted state depends on it never changing.
    const PINNED_A: u64 = 0x089b_c907_b544_c769;

    #[test]
    fn stable_hash_is_stable_and_separates_parts() {
        assert_eq!(stable_hash(&[b"goro"]), stable_hash(&[b"goro"]));
        assert_ne!(stable_hash(&[b"ab", b"c"]), stable_hash(&[b"a", b"bc"]));
        // Pinned: persisted state depends on this value never changing.
        assert_eq!(stable_hash(&[b"a"]), PINNED_A);
    }

    #[test]
    fn repo_state_round_trips_per_repository() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        let a = Path::new("/work/a");
        assert_eq!(store.repo_state(a), RepoState::default());
        let state = RepoState {
            seen: Some(HashMap::from([(
                "src/x.rs".to_string(),
                HashSet::from([1, 2]),
            )])),
            reviewed: HashSet::from([42]),
        };
        store.save_repo_state(a, &state).unwrap();
        assert_eq!(store.repo_state(a), state);
        assert_eq!(store.repo_state(Path::new("/work/b")), RepoState::default());
    }

    #[test]
    fn corrupt_state_reads_as_default() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        let root = Path::new("/work/a");
        let file = store.repo_file(root);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "{not json").unwrap();
        assert_eq!(store.repo_state(root), RepoState::default());
    }

    #[test]
    fn recent_repositories_are_most_recent_first_and_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        for root in ["/a", "/b", "/a"] {
            store.touch_recent(Path::new(root)).unwrap();
        }
        let roots: Vec<_> = store.recent().into_iter().map(|r| r.root).collect();
        assert_eq!(roots, [PathBuf::from("/a"), PathBuf::from("/b")]);
    }
}
