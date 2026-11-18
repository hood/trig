use std::borrow::Cow;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::Result;
use rayon::prelude::*;
use regex::bytes::{Regex, RegexBuilder};
use rustc_hash::FxHashMap;

use crate::plan::{self, Q};
use crate::simd;
use crate::store::{FLAG_SKIPPED, Store};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    Sensitive,
    Insensitive,
    Smart,
}

#[derive(Clone, Debug)]
pub struct Opts {
    pub case: Case,
    pub fixed: bool,
    pub max: usize,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { case: Case::Smart, fixed: false, max: 10_000 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Hit {
    pub path: String,
    pub line: u32,
    pub col: u32,
    /// Byte length of the match within `text` (clamped to the stored text).
    pub len: u32,
    pub text: String,
}

impl Hit {
    pub fn vimgrep(&self, out: &mut String) {
        use std::fmt::Write;
        let _ = writeln!(out, "{}:{}:{}:{}", self.path, self.line, self.col, self.text);
    }

    /// rg --color=always look: purple path, green line, bold red match.
    pub fn vimgrep_color(&self, out: &mut String) {
        use std::fmt::Write;
        let s = (self.col as usize - 1).min(self.text.len());
        let e = (s + self.len as usize).min(self.text.len());
        let (a, m, b) = match (self.text.get(..s), self.text.get(s..e), self.text.get(e..)) {
            (Some(a), Some(m), Some(b)) => (a, m, b),
            _ => (self.text.as_str(), "", ""),
        };
        let _ = writeln!(out, "\x1b[35m{}\x1b[0m:\x1b[32m{}\x1b[0m:{}:{a}\x1b[1;31m{m}\x1b[0m{b}", self.path, self.line, self.col);
    }
}

#[derive(Debug, Default)]
pub struct Stats {
    pub plan: String,
    pub docs_total: usize,
    pub candidates: usize,
    pub matched_files: usize,
    pub plan_us: u128,
    pub verify_us: u128,
    pub truncated: bool,
}

const MAX_LINE: usize = 400;

fn build_regex(pattern: &str, opts: &Opts) -> Result<(Regex, String, bool)> {
    let ci = match opts.case {
        Case::Sensitive => false,
        Case::Insensitive => true,
        Case::Smart => !pattern.chars().any(char::is_uppercase),
    };
    let mk = |p: &str| RegexBuilder::new(p).case_insensitive(ci).multi_line(true).size_limit(64 << 20).build();
    if opts.fixed {
        let p = regex::escape(pattern);
        return Ok((mk(&p)?, p, ci));
    }
    match mk(pattern) {
        Ok(r) => Ok((r, pattern.to_string(), ci)),
        // Half-typed regex in a live picker: search it literally instead of erroring.
        Err(_) => {
            let p = regex::escape(pattern);
            Ok((mk(&p)?, p, ci))
        }
    }
}

fn eval<'a>(q: &Q, lists: &'a FxHashMap<u32, Vec<u32>>) -> Option<Cow<'a, [u32]>> {
    match q {
        Q::All => None,
        Q::Tri(t) => Some(Cow::Borrowed(lists.get(t).map_or(&[][..], |v| &v[..]))),
        Q::And(v) => {
            let mut rs: Vec<Cow<[u32]>> = v.iter().filter_map(|c| eval(c, lists)).collect();
            if rs.is_empty() {
                return None;
            }
            rs.sort_unstable_by_key(|r| r.len());
            let mut acc = rs[0].clone();
            for r in &rs[1..] {
                if acc.is_empty() {
                    break;
                }
                let mut out = Vec::with_capacity(acc.len());
                simd::intersect(&acc, r, &mut out);
                acc = Cow::Owned(out);
            }
            Some(acc)
        }
        Q::Or(v) => {
            let mut acc: Vec<u32> = Vec::new();
            for c in v {
                let r = eval(c, lists)?;
                let mut out = Vec::new();
                simd::union(&acc, &r, &mut out);
                acc = out;
            }
            Some(Cow::Owned(acc))
        }
    }
}

/// Candidate doc ids for a plan (`None` = every doc), tombstones removed.
pub fn candidates(store: &Store, q: &Q) -> Result<Option<Vec<u32>>> {
    let mut tris = Vec::new();
    q.trigrams(&mut tris);
    tris.sort_unstable();
    tris.dedup();
    let lists: FxHashMap<u32, Vec<u32>> = tris.iter().copied().zip(store.postings(&tris)?).collect();
    let Some(ids) = eval(q, &lists) else { return Ok(None) };
    let tomb = store.tomb.read();
    Ok(Some(ids.iter().copied().filter(|id| !tomb.contains(*id)).collect()))
}

fn verify_file(re: &Regex, data: &[u8], path: &str, found: &AtomicUsize, max: usize, out: &mut Vec<Hit>) {
    // rg strips a UTF-8 BOM before matching, so columns on line 1 line up with it.
    let data = data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(data);
    let (mut pos, mut line_no, mut counted) = (0usize, 1u32, 0usize);
    while pos <= data.len() {
        let Some(m) = re.find_at(data, pos) else { break };
        let ls = memchr::memrchr(b'\n', &data[..m.start()]).map_or(0, |i| i + 1);
        let le = memchr::memchr(b'\n', &data[m.start()..]).map_or(data.len(), |i| m.start() + i);
        line_no += simd::count_byte(&data[counted..ls], b'\n') as u32;
        counted = ls;
        let line = &data[ls..le];
        // rg semantics: a match has to fit in one line.
        if let Some(lm) = re.find(line) {
            let mut text = line.strip_suffix(b"\r").unwrap_or(line);
            if text.len() > MAX_LINE {
                text = &text[..MAX_LINE];
            }
            out.push(Hit { path: path.to_string(), line: line_no, col: lm.start() as u32 + 1, len: lm.len() as u32, text: String::from_utf8_lossy(text).into_owned() });
            if found.fetch_add(1, Ordering::Relaxed) + 1 >= max {
                return;
            }
        }
        pos = le + 1;
    }
}

pub fn search(store: &Store, root: &Path, pattern: &str, opts: &Opts) -> Result<(Vec<Hit>, Stats)> {
    let mut st = Stats::default();
    if pattern.is_empty() {
        return Ok((Vec::new(), st));
    }
    let t0 = Instant::now();
    let (re, pat, ci) = build_regex(pattern, opts)?;
    let q = plan::plan(&pat, ci)?;
    st.plan = format!("{q:?}");

    let mut paths: Vec<String> = Vec::new();
    match candidates(store, &q)? {
        Some(ids) => {
            for d in store.docs(&ids)?.into_iter().flatten() {
                if d.flags & FLAG_SKIPPED == 0 {
                    paths.push(d.path);
                }
            }
        }
        None => {
            let tomb = store.tomb.read();
            store.for_each_doc(|id, d| {
                if d.flags & FLAG_SKIPPED == 0 && !tomb.contains(id) {
                    paths.push(d.path);
                }
            })?;
        }
    }
    paths.sort_unstable();
    st.candidates = paths.len();
    st.plan_us = t0.elapsed().as_micros();

    let t1 = Instant::now();
    let found = AtomicUsize::new(0);
    let per_file: Vec<Vec<Hit>> = paths
        .par_iter()
        .map(|p| {
            let mut out = Vec::new();
            if found.load(Ordering::Relaxed) >= opts.max {
                return out;
            }
            if let Ok(data) = std::fs::read(root.join(p)) {
                verify_file(&re, &data, p, &found, opts.max, &mut out);
            }
            out
        })
        .collect();
    st.matched_files = per_file.iter().filter(|v| !v.is_empty()).count();
    let mut hits: Vec<Hit> = per_file.into_iter().flatten().collect();
    st.truncated = hits.len() >= opts.max;
    hits.truncate(opts.max);
    st.verify_us = t1.elapsed().as_micros();
    Ok((hits, st))
}
