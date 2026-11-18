use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};
use parking_lot::{Mutex, RwLock};
use roaring::RoaringBitmap;
use rocksdb::compaction_filter::{CompactionFilter, Decision};
use rocksdb::compaction_filter_factory::{CompactionFilterContext, CompactionFilterFactory};
use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamily, ColumnFamilyDescriptor, DB, DBCompressionType, IteratorMode,
    MergeOperands, Options, WriteBatch, WriteOptions,
};

use crate::postings;

pub const CF_DOCS: &str = "docs";
pub const CF_PATHS: &str = "paths";
pub const CF_POSTINGS: &str = "postings";
pub const CF_META: &str = "meta";

const META_NEXT_ID: &[u8] = b"next_id";
const META_TOMB: &[u8] = b"tomb";
const META_ROOT: &[u8] = b"root";

pub const FLAG_SKIPPED: u8 = 1;

#[derive(Clone, Debug, PartialEq)]
pub struct DocMeta {
    pub mtime: u64,
    pub size: u64,
    pub hash: u64,
    pub flags: u8,
    pub path: String,
}

impl DocMeta {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(25 + self.path.len());
        v.extend_from_slice(&self.mtime.to_le_bytes());
        v.extend_from_slice(&self.size.to_le_bytes());
        v.extend_from_slice(&self.hash.to_le_bytes());
        v.push(self.flags);
        v.extend_from_slice(self.path.as_bytes());
        v
    }

    pub fn decode(b: &[u8]) -> Self {
        let u = |r: std::ops::Range<usize>| u64::from_le_bytes(b[r].try_into().unwrap());
        DocMeta {
            mtime: u(0..8),
            size: u(8..16),
            hash: u(16..24),
            flags: b[24],
            path: String::from_utf8_lossy(&b[25..]).into_owned(),
        }
    }
}

pub fn tri_key(t: u32) -> [u8; 3] {
    [(t >> 16) as u8, (t >> 8) as u8, t as u8]
}

pub fn doc_key(id: u32) -> [u8; 4] {
    id.to_be_bytes()
}

/// Associative union. Also drops tombstoned ids: rocksdb doesn't run the compaction filter
/// on merge results, so this is what cleans keys that still had pending operands.
fn merge_postings(tomb: &RwLock<RoaringBitmap>, existing: Option<&[u8]>, ops: &MergeOperands) -> Vec<u8> {
    let mut lists = Vec::with_capacity(ops.len() + 1);
    if let Some(e) = existing {
        lists.push(postings::decode(e));
    }
    for op in ops {
        lists.push(postings::decode(op));
    }
    let mut merged = postings::merge_lists(lists);
    let t = tomb.read();
    if !t.is_empty() {
        merged.retain(|id| !t.contains(*id));
    }
    postings::encode(&merged)
}

/// Drops tombstoned doc ids from posting lists while compacting.
struct TombFilter {
    tomb: RoaringBitmap,
    buf: Vec<u8>,
    name: CString,
}

impl CompactionFilter for TombFilter {
    fn filter(&mut self, _level: u32, _key: &[u8], value: &[u8]) -> Decision {
        if self.tomb.is_empty() {
            return Decision::Keep;
        }
        let ids = postings::decode(value);
        let kept: Vec<u32> = ids.iter().copied().filter(|id| !self.tomb.contains(*id)).collect();
        if kept.len() == ids.len() {
            Decision::Keep
        } else if kept.is_empty() {
            Decision::Remove
        } else {
            self.buf = postings::encode(&kept);
            // SAFETY: rocksdb's C shim copies the new value (`new_value->assign`) right after
            // the callback returns, before this filter is called again or dropped.
            Decision::Change(unsafe { &*(self.buf.as_slice() as *const [u8]) })
        }
    }

    fn name(&self) -> &CStr {
        &self.name
    }
}

struct TombFilterFactory {
    tomb: Arc<RwLock<RoaringBitmap>>,
    name: CString,
}

impl CompactionFilterFactory for TombFilterFactory {
    type Filter = TombFilter;

    fn create(&mut self, _ctx: CompactionFilterContext) -> TombFilter {
        TombFilter { tomb: self.tomb.read().clone(), buf: Vec::new(), name: c"trig.tomb".to_owned() }
    }

    fn name(&self) -> &CStr {
        &self.name
    }
}

pub struct Store {
    pub db: DB,
    pub tomb: Arc<RwLock<RoaringBitmap>>,
    pub next_id: AtomicU32,
    pub write_lock: Mutex<()>,
    pub read_only: bool,
}

impl Store {
    pub fn open(path: &Path, read_only: bool) -> Result<Self> {
        let tomb = Arc::new(RwLock::new(RoaringBitmap::new()));
        let cache = Cache::new_lru_cache(256 << 20);
        let mut bb = BlockBasedOptions::default();
        bb.set_block_cache(&cache);
        bb.set_bloom_filter(10.0, false);
        bb.set_cache_index_and_filter_blocks(true);
        bb.set_pin_l0_filter_and_index_blocks_in_cache(true);

        let mut base = Options::default();
        base.set_block_based_table_factory(&bb);
        base.set_compression_type(DBCompressionType::Lz4);
        base.set_bottommost_compression_type(DBCompressionType::Zstd);

        let mut post = base.clone();
        let mt = tomb.clone();
        post.set_merge_operator_associative("trig.union", move |_k: &[u8], e: Option<&[u8]>, ops: &MergeOperands| {
            Some(merge_postings(&mt, e, ops))
        });
        post.set_compaction_filter_factory(TombFilterFactory { tomb: tomb.clone(), name: c"trig.tomb.factory".to_owned() });
        post.set_write_buffer_size(256 << 20);
        post.set_max_write_buffer_number(4);
        post.set_target_file_size_base(64 << 20);

        let cfs = vec![
            ColumnFamilyDescriptor::new(CF_DOCS, base.clone()),
            ColumnFamilyDescriptor::new(CF_PATHS, base.clone()),
            ColumnFamilyDescriptor::new(CF_POSTINGS, post),
            ColumnFamilyDescriptor::new(CF_META, base.clone()),
        ];

        let mut db_opts = base;
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);
        db_opts.increase_parallelism(std::thread::available_parallelism().map_or(4, |n| n.get()) as i32);
        db_opts.set_max_background_jobs(6);
        db_opts.set_atomic_flush(true);
        db_opts.set_max_open_files(512);

        let db = if read_only {
            DB::open_cf_descriptors_read_only(&db_opts, path, cfs, false)
        } else {
            std::fs::create_dir_all(path)?;
            DB::open_cf_descriptors(&db_opts, path, cfs)
        }
        .with_context(|| format!("open rocksdb at {}", path.display()))?;

        let store = Store { db, tomb, next_id: AtomicU32::new(0), write_lock: Mutex::new(()), read_only };
        let meta = store.cf(CF_META);
        if let Some(v) = store.db.get_pinned_cf(meta, META_NEXT_ID)? {
            store.next_id.store(u32::from_le_bytes(v[..4].try_into()?), Ordering::SeqCst);
        }
        if let Some(v) = store.db.get_pinned_cf(meta, META_TOMB)? {
            *store.tomb.write() = RoaringBitmap::deserialize_from(&v[..])?;
        }
        Ok(store)
    }

    pub fn cf(&self, name: &str) -> &ColumnFamily {
        self.db.cf_handle(name).expect("column family")
    }

    pub fn root(&self) -> Result<Option<String>> {
        Ok(self.db.get_cf(self.cf(CF_META), META_ROOT)?.map(|v| String::from_utf8_lossy(&v).into_owned()))
    }

    pub fn set_root(&self, root: &str) -> Result<()> {
        Ok(self.db.put_cf(self.cf(CF_META), META_ROOT, root)?)
    }

    pub fn alloc_ids(&self, n: u32) -> u32 {
        self.next_id.fetch_add(n, Ordering::SeqCst)
    }

    /// Posting lists for `trigrams`, decoded. Missing keys come back empty.
    pub fn postings(&self, trigrams: &[u32]) -> Result<Vec<Vec<u32>>> {
        let keys: Vec<[u8; 3]> = trigrams.iter().map(|&t| tri_key(t)).collect();
        let cf = self.cf(CF_POSTINGS);
        let mut out = Vec::with_capacity(keys.len());
        for r in self.db.batched_multi_get_cf(cf, &keys, false) {
            out.push(match r? {
                Some(v) => postings::decode(&v),
                None => Vec::new(),
            });
        }
        Ok(out)
    }

    pub fn posting_lens(&self, trigrams: &[u32]) -> Result<Vec<usize>> {
        let keys: Vec<[u8; 3]> = trigrams.iter().map(|&t| tri_key(t)).collect();
        let cf = self.cf(CF_POSTINGS);
        self.db
            .batched_multi_get_cf(cf, &keys, false)
            .into_iter()
            .map(|r| Ok(r?.map_or(0, |v| postings::len(&v))))
            .collect()
    }

    pub fn docs(&self, ids: &[u32]) -> Result<Vec<Option<DocMeta>>> {
        let keys: Vec<[u8; 4]> = ids.iter().map(|&i| doc_key(i)).collect();
        let cf = self.cf(CF_DOCS);
        self.db
            .batched_multi_get_cf(cf, &keys, false)
            .into_iter()
            .map(|r| Ok(r?.map(|v| DocMeta::decode(&v))))
            .collect()
    }

    pub fn for_each_doc(&self, mut f: impl FnMut(u32, DocMeta)) -> Result<()> {
        for kv in self.db.iterator_cf(self.cf(CF_DOCS), IteratorMode::Start) {
            let (k, v) = kv?;
            f(u32::from_be_bytes(k[..4].try_into()?), DocMeta::decode(&v));
        }
        Ok(())
    }

    pub fn path_index(&self) -> Result<HashMap<String, (u32, DocMeta)>> {
        let mut m = HashMap::new();
        self.for_each_doc(|id, d| {
            m.insert(d.path.clone(), (id, d));
        })?;
        Ok(m)
    }

    pub fn doc_id_for(&self, path: &str) -> Result<Option<u32>> {
        Ok(self.db.get_pinned_cf(self.cf(CF_PATHS), path)?.map(|v| u32::from_le_bytes(v[..4].try_into().unwrap())))
    }

    /// Commit doc table changes + tombstones atomically. `replaced` ids get tombstoned.
    pub fn commit_docs(&self, docs: &[(u32, DocMeta)], replaced: &[u32], removed_paths: &[String], wal: bool) -> Result<()> {
        let _g = self.write_lock.lock();
        let (cd, cp, cm) = (self.cf(CF_DOCS), self.cf(CF_PATHS), self.cf(CF_META));
        let mut b = WriteBatch::default();
        for (id, d) in docs {
            b.put_cf(cd, doc_key(*id), d.encode());
            b.put_cf(cp, &d.path, id.to_le_bytes());
        }
        for p in removed_paths {
            b.delete_cf(cp, p);
        }
        let mut tomb = self.tomb.write();
        for &id in replaced {
            b.delete_cf(cd, doc_key(id));
            tomb.insert(id);
        }
        let mut tb = Vec::with_capacity(tomb.serialized_size());
        tomb.serialize_into(&mut tb)?;
        drop(tomb);
        b.put_cf(cm, META_TOMB, tb);
        b.put_cf(cm, META_NEXT_ID, self.next_id.load(Ordering::SeqCst).to_le_bytes());
        let mut wo = WriteOptions::default();
        wo.disable_wal(!wal);
        self.db.write_opt(b, &wo)?;
        Ok(())
    }

    pub fn write_postings(&self, lists: impl IntoIterator<Item = (u32, Vec<u8>)>, wal: bool) -> Result<()> {
        let cf = self.cf(CF_POSTINGS);
        let mut b = WriteBatch::default();
        for (t, enc) in lists {
            b.merge_cf(cf, tri_key(t), enc);
        }
        let mut wo = WriteOptions::default();
        wo.disable_wal(!wal);
        self.db.write_opt(b, &wo)?;
        Ok(())
    }

    pub fn flush(&self) -> Result<()> {
        let cfs: Vec<&ColumnFamily> = [CF_DOCS, CF_PATHS, CF_POSTINGS, CF_META].iter().map(|n| self.cf(n)).collect();
        self.db.flush_cfs_opt(&cfs, &rocksdb::FlushOptions::default())?;
        Ok(())
    }

    /// Fold merge operands and drop tombstoned ids. Afterwards those ids are gone from every
    /// posting list, so they can leave the tombstone set too.
    pub fn compact(&self) -> Result<()> {
        let snapshot = self.tomb.read().clone();
        self.flush()?;
        let mut co = rocksdb::CompactOptions::default();
        co.set_bottommost_level_compaction(rocksdb::BottommostLevelCompaction::Force);
        self.db.compact_range_cf_opt(self.cf(CF_POSTINGS), None::<&[u8]>, None::<&[u8]>, &co);
        {
            let _g = self.write_lock.lock();
            let mut t = self.tomb.write();
            *t -= &snapshot;
            let mut tb = Vec::new();
            t.serialize_into(&mut tb)?;
            self.db.put_cf(self.cf(CF_META), META_TOMB, tb)?;
        }
        Ok(())
    }

    pub fn property(&self, cf: &str, name: &str) -> Option<u64> {
        self.db.property_int_value_cf(self.cf(cf), name).ok().flatten()
    }
}
