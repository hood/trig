pub mod extract;
pub mod index;
pub mod plan;
pub mod postings;
pub mod search;
pub mod simd;
pub mod store;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub use index::{Progress, ReconcileStats, state};
pub use search::{Case, Hit, Opts, Stats};
pub use store::Store;

/// `$XDG_CACHE_HOME/nvim/trig/<xxh3(root)>`, matching `stdpath('cache')` on unix.
pub fn default_db_path(root: &Path) -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"));
    let h = xxhash_rust::xxh3::xxh3_64(root.as_os_str().as_encoded_bytes());
    cache.join("nvim").join("trig").join(format!("{h:016x}"))
}

pub struct Engine {
    pub root: PathBuf,
    pub db_path: PathBuf,
    pub store: Store,
    pub progress: Progress,
}

impl Engine {
    pub fn open(root: &Path, db: Option<&Path>, read_only: bool) -> Result<Self> {
        let root = root.canonicalize().with_context(|| format!("root {}", root.display()))?;
        let db_path = db.map(PathBuf::from).unwrap_or_else(|| default_db_path(&root));
        let store = Store::open(&db_path, read_only)?;
        Ok(Engine { root, db_path, store, progress: Progress::default() })
    }

    /// Path relative to root, or None when it lives outside it.
    pub fn rel(&self, p: &Path) -> Option<String> {
        let abs = if p.is_absolute() { p.to_path_buf() } else { self.root.join(p) };
        let canon = abs.canonicalize().ok().or_else(|| {
            let parent = abs.parent()?.canonicalize().ok()?;
            Some(parent.join(abs.file_name()?))
        })?;
        canon.strip_prefix(&self.root).ok().map(|r| r.to_string_lossy().into_owned())
    }

    pub fn reconcile(&self, compact: bool) -> Result<ReconcileStats> {
        let r = index::reconcile(&self.store, &self.root, &self.progress, compact);
        if r.is_err() {
            self.progress.state.store(state::ERROR, std::sync::atomic::Ordering::SeqCst);
        }
        r
    }

    pub fn update(&self, p: &Path) -> Result<bool> {
        let Some(rel) = self.rel(p) else { return Ok(false) };
        if !self.root.join(&rel).is_file() {
            index::remove_one(&self.store, &rel)?;
            return Ok(true);
        }
        index::update_one(&self.store, &self.root, &rel, &self.progress)?;
        Ok(true)
    }

    pub fn remove(&self, p: &Path) -> Result<bool> {
        let Some(rel) = self.rel(p) else { return Ok(false) };
        index::remove_one(&self.store, &rel)?;
        Ok(true)
    }

    pub fn search(&self, pattern: &str, opts: &Opts) -> Result<(Vec<Hit>, Stats)> {
        search::search(&self.store, &self.root, pattern, opts)
    }
}
