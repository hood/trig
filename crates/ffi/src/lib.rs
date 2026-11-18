//! C ABI for LuaJIT FFI. Every entry point catches panics: unwinding into nvim would abort it.
use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;

use trig_core::{Case, Engine, Opts};

thread_local! {
    static LAST_ERR: RefCell<CString> = RefCell::new(CString::default());
}

fn set_err(msg: impl std::fmt::Display) {
    let s = CString::new(msg.to_string().replace('\0', " ")).unwrap_or_default();
    LAST_ERR.with(|e| *e.borrow_mut() = s);
}

fn guard<T>(fallback: T, f: impl FnOnce() -> anyhow::Result<T>) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            set_err(format!("{e:#}"));
            fallback
        }
        Err(_) => {
            set_err("panic in trig");
            fallback
        }
    }
}

unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() { None } else { unsafe { CStr::from_ptr(p) }.to_str().ok() }
}

pub struct Handle {
    engine: Arc<Engine>,
    indexer: parking_lot::Mutex<Option<JoinHandle<()>>>,
    db_path: CString,
    root: CString,
}

#[repr(C)]
pub struct TrigStatus {
    pub state: i32,
    pub done: u64,
    pub total: u64,
}

pub const FLAG_IGNORE_CASE: u32 = 1;
pub const FLAG_CASE_SENSITIVE: u32 = 2;
pub const FLAG_FIXED: u32 = 4;

#[unsafe(no_mangle)]
pub extern "C" fn trig_last_error() -> *const c_char {
    LAST_ERR.with(|e| e.borrow().as_ptr())
}

/// `db` may be NULL for the default cache location.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trig_open(root: *const c_char, db: *const c_char) -> *mut Handle {
    guard(std::ptr::null_mut(), || {
        let root = unsafe { cstr(root) }.ok_or_else(|| anyhow::anyhow!("root is null or not utf-8"))?;
        let db = unsafe { cstr(db) }.map(Path::new);
        let engine = Engine::open(Path::new(root), db, false)?;
        let h = Handle {
            db_path: CString::new(engine.db_path.to_string_lossy().as_bytes())?,
            root: CString::new(engine.root.to_string_lossy().as_bytes())?,
            engine: Arc::new(engine),
            indexer: parking_lot::Mutex::new(None),
        };
        Ok(Box::into_raw(Box::new(h)))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn trig_close(h: *mut Handle) {
    if !h.is_null() {
        // A running indexer keeps its own Arc; the process exiting is fine for rocksdb.
        drop(unsafe { Box::from_raw(h) });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn trig_db_path(h: *const Handle) -> *const c_char {
    unsafe { &*h }.db_path.as_ptr()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn trig_root(h: *const Handle) -> *const c_char {
    unsafe { &*h }.root.as_ptr()
}

/// Starts a background reconcile. Returns 1 if started, 0 if one is already running.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trig_index_async(h: *const Handle, compact: i32) -> i32 {
    let h = unsafe { &*h };
    guard(-1, || {
        let mut slot = h.indexer.lock();
        if slot.as_ref().is_some_and(|j| !j.is_finished()) {
            return Ok(0);
        }
        let e = h.engine.clone();
        *slot = Some(std::thread::Builder::new().name("trig-index".into()).spawn(move || {
            if let Err(err) = e.reconcile(compact != 0) {
                set_err(format!("{err:#}"));
                e.progress.state.store(trig_core::state::ERROR, Ordering::SeqCst);
                eprintln!("trig: index failed: {err:#}");
            }
        })?);
        Ok(1)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn trig_status(h: *const Handle, out: *mut TrigStatus) {
    let p = &unsafe { &*h }.engine.progress;
    unsafe {
        *out = TrigStatus {
            state: p.state.load(Ordering::SeqCst) as i32,
            done: p.done.load(Ordering::Relaxed),
            total: p.total.load(Ordering::Relaxed),
        }
    };
}

/// 1 = indexed, 0 = outside root, -1 = error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trig_update(h: *const Handle, path: *const c_char) -> i32 {
    let h = unsafe { &*h };
    guard(-1, || {
        let p = unsafe { cstr(path) }.ok_or_else(|| anyhow::anyhow!("bad path"))?;
        Ok(h.engine.update(Path::new(p))? as i32)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn trig_remove(h: *const Handle, path: *const c_char) -> i32 {
    let h = unsafe { &*h };
    guard(-1, || {
        let p = unsafe { cstr(path) }.ok_or_else(|| anyhow::anyhow!("bad path"))?;
        Ok(h.engine.remove(Path::new(p))? as i32)
    })
}

/// Returns `path:line:col:text\n` lines (paths relative to root) in a buffer owned by Rust:
/// release it with `trig_free(ptr, len)`. NULL on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trig_search(h: *const Handle, pattern: *const c_char, flags: u32, max: u32, out_len: *mut usize) -> *mut u8 {
    let h = unsafe { &*h };
    guard(std::ptr::null_mut(), || {
        let pat = unsafe { cstr(pattern) }.ok_or_else(|| anyhow::anyhow!("bad pattern"))?;
        let case = if flags & FLAG_IGNORE_CASE != 0 {
            Case::Insensitive
        } else if flags & FLAG_CASE_SENSITIVE != 0 {
            Case::Sensitive
        } else {
            Case::Smart
        };
        let opts = Opts { case, fixed: flags & FLAG_FIXED != 0, max: if max == 0 { 10_000 } else { max as usize } };
        let (hits, _) = h.engine.search(pat, &opts)?;
        let mut s = String::with_capacity(hits.len() * 80);
        for hit in &hits {
            hit.vimgrep(&mut s);
        }
        let b = s.into_bytes().into_boxed_slice();
        unsafe { *out_len = b.len() };
        Ok(Box::into_raw(b) as *mut u8)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn trig_free(p: *mut u8, len: usize) {
    if !p.is_null() {
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(p, len)) });
    }
}
