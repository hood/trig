//! Regex -> trigram query. The result is a *necessary* condition: every file that can match
//! the regex satisfies the query. False positives are fine, the verify pass drops them.
use std::collections::BTreeSet;
use std::fmt;

use anyhow::Result;
use regex_syntax::hir::{Class, Hir, HirKind};

use crate::extract::trigrams_of;

/// Max size of an exact string set before we give up on cross products.
const SET_CAP: usize = 64;
/// Max chars in a class we still expand into alternatives (covers case folding).
const CLASS_CAP: usize = 16;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Q {
    All,
    Tri(u32),
    And(Vec<Q>),
    Or(Vec<Q>),
}

impl fmt::Debug for Q {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Q::All => write!(f, "*"),
            Q::Tri(t) => {
                let b = [(t >> 16) as u8, (t >> 8) as u8, *t as u8];
                write!(f, "{:?}", String::from_utf8_lossy(&b))
            }
            Q::And(v) => write!(f, "and{v:?}"),
            Q::Or(v) => write!(f, "or{v:?}"),
        }
    }
}

impl Q {
    pub fn trigrams(&self, out: &mut Vec<u32>) {
        match self {
            Q::All => {}
            Q::Tri(t) => out.push(*t),
            Q::And(v) | Q::Or(v) => v.iter().for_each(|q| q.trigrams(out)),
        }
    }
}

pub fn and(v: Vec<Q>) -> Q {
    let mut flat = BTreeSet::new();
    for q in v {
        match q {
            Q::All => {}
            Q::And(inner) => flat.extend(inner),
            q => {
                flat.insert(q);
            }
        }
    }
    match flat.len() {
        0 => Q::All,
        1 => flat.into_iter().next().unwrap(),
        _ => Q::And(flat.into_iter().collect()),
    }
}

pub fn or(v: Vec<Q>) -> Q {
    let mut flat = BTreeSet::new();
    for q in v {
        match q {
            Q::All => return Q::All,
            Q::Or(inner) => flat.extend(inner),
            q => {
                flat.insert(q);
            }
        }
    }
    match flat.len() {
        0 => Q::All,
        1 => flat.into_iter().next().unwrap(),
        _ => Q::Or(flat.into_iter().collect()),
    }
}

type Set = BTreeSet<Vec<u8>>;

struct Info {
    /// When small, the exact set of strings this node can match.
    exact: Option<Set>,
    /// Extra necessary condition on top of `exact`.
    q: Q,
}

fn conv(set: &Set) -> Q {
    or(set.iter().map(|s| and(trigrams_of(s).map(Q::Tri).collect())).collect())
}

fn lower(mut s: Vec<u8>) -> Vec<u8> {
    s.make_ascii_lowercase();
    s
}

fn exact(set: Set) -> Info {
    Info { exact: Some(set), q: Q::All }
}

fn empty() -> Info {
    exact(BTreeSet::from([Vec::new()]))
}

fn all() -> Info {
    Info { exact: None, q: Q::All }
}

fn close(i: Info) -> Q {
    match i.exact {
        Some(s) => and(vec![i.q, conv(&s)]),
        None => i.q,
    }
}

fn class(c: &Class) -> Info {
    let mut set = Set::new();
    match c {
        Class::Unicode(u) => {
            let n: u32 = u.ranges().iter().map(|r| r.end() as u32 - r.start() as u32 + 1).sum();
            if n as usize > CLASS_CAP {
                return all();
            }
            for r in u.ranges() {
                for cp in r.start() as u32..=r.end() as u32 {
                    if let Some(ch) = char::from_u32(cp) {
                        let mut b = [0; 4];
                        set.insert(lower(ch.encode_utf8(&mut b).as_bytes().to_vec()));
                    }
                }
            }
        }
        Class::Bytes(by) => {
            let n: usize = by.ranges().iter().map(|r| (r.end() - r.start()) as usize + 1).sum();
            if n > CLASS_CAP {
                return all();
            }
            for r in by.ranges() {
                for b in r.start()..=r.end() {
                    set.insert(vec![b.to_ascii_lowercase()]);
                }
            }
        }
    }
    exact(set)
}

fn info(h: &Hir) -> Info {
    match h.kind() {
        HirKind::Empty | HirKind::Look(_) => empty(),
        HirKind::Literal(l) => exact(BTreeSet::from([lower(l.0.to_vec())])),
        HirKind::Class(c) => class(c),
        HirKind::Capture(c) => info(&c.sub),
        HirKind::Repetition(r) => {
            if r.min == 0 {
                all()
            } else if r.min == 1 && r.max == Some(1) {
                info(&r.sub)
            } else {
                Info { exact: None, q: close(info(&r.sub)) }
            }
        }
        HirKind::Concat(hs) => {
            let mut acc = empty();
            for h in hs {
                let c = info(h);
                acc = match (acc.exact, c.exact) {
                    (Some(a), Some(b)) if a.len() * b.len() <= SET_CAP => {
                        let mut x = Set::new();
                        for p in &a {
                            for s in &b {
                                let mut v = p.clone();
                                v.extend_from_slice(s);
                                x.insert(v);
                            }
                        }
                        Info { exact: Some(x), q: and(vec![acc.q, c.q]) }
                    }
                    (a, b) => {
                        // Can't keep the cross product: freeze the left side into the query
                        // and restart the exact set from the right (if it has one).
                        let left = close(Info { exact: a, q: acc.q });
                        Info { exact: b, q: and(vec![left, c.q]) }
                    }
                };
            }
            acc
        }
        HirKind::Alternation(hs) => {
            let infos: Vec<Info> = hs.iter().map(info).collect();
            let total: Option<usize> = infos.iter().map(|i| i.exact.as_ref().map(Set::len)).sum();
            match total {
                Some(n) if n <= SET_CAP => {
                    let mut set = Set::new();
                    let mut qs = Vec::new();
                    for i in infos {
                        set.extend(i.exact.unwrap());
                        qs.push(i.q);
                    }
                    Info { exact: Some(set), q: or(qs) }
                }
                _ => Info { exact: None, q: or(infos.into_iter().map(close).collect()) },
            }
        }
    }
}

pub fn plan_hir(h: &Hir) -> Q {
    close(info(h))
}

pub fn plan(pattern: &str, case_insensitive: bool) -> Result<Q> {
    let hir = regex_syntax::ParserBuilder::new().case_insensitive(case_insensitive).multi_line(true).build().parse(pattern)?;
    Ok(plan_hir(&hir))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> String {
        format!("{:?}", plan(s, false).unwrap())
    }

    #[test]
    fn shapes() {
        assert_eq!(p("ab"), "*");
        assert_eq!(p("abc"), r#""abc""#);
        assert_eq!(p("abcd"), r#"and["abc", "bcd"]"#);
        assert_eq!(p("Foo"), r#""foo""#);
        assert_eq!(p("abc|xyz"), r#"or["abc", "xyz"]"#);
        assert_eq!(p("abc.*xyz"), r#"and["abc", "xyz"]"#);
        assert_eq!(p("ab(c|d)e"), r#"or[and["abc", "bce"], and["abd", "bde"]]"#);
        assert_eq!(p("(abc)+def"), r#"and["abc", "def"]"#);
        assert_eq!(p("x?abc"), r#""abc""#);
        assert_eq!(p(".*"), "*");
        assert_eq!(p("abc|.*"), "*");
        assert_eq!(p("[a-z]+_init"), r#"and["_in", "ini", "nit"]"#);
    }

    #[test]
    fn case_insensitive_folds_back_to_lowercase() {
        assert_eq!(format!("{:?}", plan("foo", true).unwrap()), r#""foo""#);
    }
}
