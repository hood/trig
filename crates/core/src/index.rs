use std::path::Path;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Instant, UNIX_EPOCH};

use anyhow::Result;
use parking_lot::Mutex;
use rayon::prelude::*;
use rustc_hash::FxHashMap;

use crate::extract::{self, Extractor};
use crate::postings;
use crate::store::{DocMeta, FLAG_SKIPPED, Store};

const CHUNK: usize = 1024;
/// Above this many changed files, skip the WAL and flush once at the end.
const BULK: usize = 2000;

#[derive(Default)]
pub struct Progress {
    pub state: AtomicU8,
    pub done: AtomicU64,
    pub total: AtomicU64,
}

pub mod state {
    pub const IDLE: u8 = 0;
    pub const WALKING: u8 = 1;
    pub const INDEXING: u8 = 2;
    pub const COMPACTING: u8 = 3;
    pub const READY: u8 = 4;
    pub const ERROR: u8 = 5;
}

#[derive(Debug, Default)]
pub struct ReconcileStats {
    pub walked: usize,
    pub changed: usize,
    pub removed: usize,
    pub walk_ms: u128,
    pub index_ms: u128,
    pub compact_ms: u128,
}

pub struct Walked {
    pub rel: String,
    pub mtime: u64,
    pub size: u64,
}

fn mtime_ns(md: &std::fs::Metadata) -> u64 {
    md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as u64)
}

struct Sink<'a> {
    out: &'a Mutex<Vec<Walked>>,
    local: Vec<Walked>,
}

impl Drop for Sink<'_> {
    fn drop(&mut self) {
        self.out.lock().append(&mut self.local);
    }
}

/// Parallel walk honoring .gitignore (same rules as rg), hidden files included, .git skipped.
pub fn walk(root: &Path) -> Vec<Walked> {
    let out = Mutex::new(Vec::new());
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build_parallel()
        .run(|| {
            let mut sink = Sink { out: &out, local: Vec::new() };
            Box::new(move |r| {
                if let Ok(e) = r {
                    if e.file_type().is_some_and(|t| t.is_file()) {
                        if let (Ok(md), Ok(rel)) = (e.metadata(), e.path().strip_prefix(root)) {
                            sink.local.push(Walked { rel: rel.to_string_lossy().into_owned(), mtime: mtime_ns(&md), size: md.len() });
                        }
                    }
                }
                ignore::WalkState::Continue
            })
        });
    out.into_inner()
}

/// A file to (re)index, with its previous doc id and content hash if it was indexed before.
pub struct Todo {
    pub rel: String,
    pub old: Option<(u32, u64)>,
}

pub fn index_files(store: &Store, root: &Path, todo: &[Todo], wal: bool, progress: &Progress) -> Result<()> {
    todo.par_chunks(CHUNK).try_for_each(|chunk| -> Result<()> {
        let mut ex = Extractor::new();
        let base = store.alloc_ids(chunk.len() as u32);
        let mut lists: FxHashMap<u32, Vec<u32>> = FxHashMap::with_capacity_and_hasher(1 << 16, Default::default());
        let mut docs = Vec::with_capacity(chunk.len());
        let mut replaced = Vec::new();
        let mut removed = Vec::new();
        for (i, t) in chunk.iter().enumerate() {
            let path = root.join(&t.rel);
            let (md, data) = match std::fs::metadata(&path).and_then(|md| Ok((md, std::fs::read(&path)?))) {
                Ok(x) => x,
                Err(_) => {
                    if let Some((old, _)) = t.old {
                        replaced.push(old);
                        removed.push(t.rel.clone());
                    }
                    continue;
                }
            };
            let hash = xxhash_rust::xxh3::xxh3_64(&data);
            let mut meta = DocMeta { mtime: mtime_ns(&md), size: md.len(), hash, flags: 0, path: t.rel.clone() };
            if let Some((old, old_hash)) = t.old {
                if old_hash == hash {
                    // touched but identical: keep the id, refresh stat info only
                    docs.push((old, meta));
                    continue;
                }
                replaced.push(old);
            }
            let id = base + i as u32;
            if md.len() > extract::MAX_FILE || extract::is_binary(&data) {
                meta.flags |= FLAG_SKIPPED;
            } else {
                for &tri in ex.trigrams(&data) {
                    lists.entry(tri).or_default().push(id);
                }
            }
            docs.push((id, meta));
        }
        // Postings first: a crash in between leaves orphan ids that verify filters out.
        store.write_postings(lists.into_iter().map(|(t, ids)| (t, postings::encode(&ids))), wal)?;
        store.commit_docs(&docs, &replaced, &removed, wal)?;
        progress.done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        Ok(())
    })
}

pub fn reconcile(store: &Store, root: &Path, progress: &Progress, compact: bool) -> Result<ReconcileStats> {
    let mut st = ReconcileStats::default();
    progress.state.store(state::WALKING, Ordering::SeqCst);
    let t0 = Instant::now();
    let mut existing = store.path_index()?;
    let walked = walk(root);
    st.walked = walked.len();
    let mut todo = Vec::new();
    for w in walked {
        match existing.remove(&w.rel) {
            Some((_, d)) if d.mtime == w.mtime && d.size == w.size => {}
            Some((id, d)) => todo.push(Todo { rel: w.rel, old: Some((id, d.hash)) }),
            None => todo.push(Todo { rel: w.rel, old: None }),
        }
    }
    let (gone_ids, gone_paths): (Vec<u32>, Vec<String>) = existing.into_iter().map(|(p, (id, _))| (id, p)).unzip();
    st.removed = gone_ids.len();
    if !gone_ids.is_empty() {
        store.commit_docs(&[], &gone_ids, &gone_paths, true)?;
    }
    st.walk_ms = t0.elapsed().as_millis();

    st.changed = todo.len();
    progress.total.store(todo.len() as u64, Ordering::SeqCst);
    progress.done.store(0, Ordering::SeqCst);
    progress.state.store(state::INDEXING, Ordering::SeqCst);
    let t1 = Instant::now();
    let bulk = todo.len() >= BULK;
    index_files(store, root, &todo, !bulk, progress)?;
    if bulk {
        store.flush()?;
    }
    store.set_root(&root.to_string_lossy())?;
    st.index_ms = t1.elapsed().as_millis();

    if compact && (bulk || st.removed > 0) {
        progress.state.store(state::COMPACTING, Ordering::SeqCst);
        let t2 = Instant::now();
        store.compact()?;
        st.compact_ms = t2.elapsed().as_millis();
    }
    progress.state.store(state::READY, Ordering::SeqCst);
    Ok(st)
}

/// Re-index one file (BufWritePost). Cheap no-op when the content hash didn't change.
pub fn update_one(store: &Store, root: &Path, rel: &str, progress: &Progress) -> Result<()> {
    let old = match store.doc_id_for(rel)? {
        Some(id) => store.docs(&[id])?.pop().flatten().map(|d| (id, d.hash)),
        None => None,
    };
    index_files(store, root, &[Todo { rel: rel.to_string(), old }], true, progress)
}

pub fn remove_one(store: &Store, rel: &str) -> Result<()> {
    if let Some(id) = store.doc_id_for(rel)? {
        store.commit_docs(&[], &[id], &[rel.to_string()], true)?;
    }
    Ok(())
}
