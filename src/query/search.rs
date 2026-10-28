//! Workspace symbol search: space separated terms, each a case-insensitive
//! substring, in any order. `key word` finds `keyword`, `KeyWord`, `key_word`
//! (ranked first) and also `word_key`. A term that is not in a symbol's own name
//! may match its container (`Foo bar` finds `Foo::bar`).

use rayon::prelude::*;

use super::Query;
use crate::base::ids::Idx;
use crate::symbols::{NameId, SymbolFlags, SymbolId};

const MAX_TERMS: usize = 16;

fn find_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let n = needle.len();
    if n == 0 || n > hay.len() {
        return None;
    }
    let first = needle[0];
    (0..=hay.len() - n).find(|&i| hay[i].to_ascii_lowercase() == first && hay[i..i + n].eq_ignore_ascii_case(needle))
}

/// Word starts: the beginning, after `_`/`:`, or a lower-to-upper transition.
fn at_boundary(hay: &[u8], pos: usize) -> bool {
    pos == 0 || matches!(hay[pos - 1], b'_' | b':') || (!hay[pos - 1].is_ascii_uppercase() && hay[pos].is_ascii_uppercase())
}

/// `name` equals `joined` when `_` is ignored and case does not matter.
fn same_ignoring_separators(name: &[u8], joined: &[u8]) -> bool {
    let mut it = joined.iter();
    for &b in name.iter().filter(|&&b| b != b'_') {
        if it.next().is_none_or(|j| !j.eq_ignore_ascii_case(&b)) {
            return false;
        }
    }
    it.next().is_none()
}

/// Matches `terms` against one name. Returns `(score, matched term mask)`.
fn score_name(name: &str, terms: &[Vec<u8>], joined: &[u8]) -> (i32, u32) {
    let hay = name.as_bytes();
    let (mut score, mut mask, mut last_end) = (0i32, 0u32, 0usize);
    let mut in_order = true;
    for (k, t) in terms.iter().enumerate() {
        let Some(pos) = find_ci(hay, t) else { continue };
        mask |= 1 << k;
        score += if pos == 0 {
            60
        } else if at_boundary(hay, pos) {
            40
        } else {
            10
        };
        if t.len() == hay.len() {
            score += 100;
        }
        in_order &= pos >= last_end;
        last_end = pos + t.len();
    }
    if mask != 0 {
        if same_ignoring_separators(hay, joined) {
            score += 1000;
        } else if in_order && terms.len() > 1 && mask.count_ones() as usize == terms.len() {
            score += 30;
        }
        score -= hay.len().min(200) as i32 / 4;
    }
    (score, mask)
}

impl Query<'_> {
    /// Best matches for `query`, best first, at most `limit` symbols.
    pub fn search_symbols(&self, query: &str, limit: usize) -> Vec<SymbolId> {
        let terms: Vec<Vec<u8>> = query.split_whitespace().take(MAX_TERMS).map(|t| t.as_bytes().to_ascii_lowercase()).collect();
        if terms.is_empty() || limit == 0 {
            return Vec::new();
        }
        let full = (1u32 << terms.len()) - 1;
        let joined: Vec<u8> = terms.concat();
        let db = &self.idx.db;

        let mut names: Vec<(i32, u32, NameId)> = (0..db.names.len())
            .into_par_iter()
            .filter_map(|i| {
                let id = NameId::from_usize(i);
                let (score, mask) = score_name(db.names.get(id), &terms, &joined);
                (mask != 0).then_some((score, mask, id))
            })
            .collect();
        // Names matching every term first, then partial matches (which need the container to match).
        names.par_sort_unstable_by(|a, b| ((b.1 == full), b.0, a.2).cmp(&((a.1 == full), a.0, b.2)));

        let live = |s: SymbolId| !db.symbol(s).flags.contains(SymbolFlags::DEAD);
        let mut out = Vec::new();
        let mut container = String::new();
        'names: for (score, mask, name) in names {
            let mut batch: Vec<(i32, SymbolId)> = Vec::new();
            let mut cands: Vec<SymbolId> = db.lookup_id(name).iter().copied().filter(|&s| live(s)).collect();
            super::drop_forward_declarations(db, &mut cands);
            for s in cands {
                let mut score = score;
                if mask != full {
                    let Some(p) = db.symbol(s).parent else { continue };
                    container.clear();
                    db.write_qualified_name(p, &mut container);
                    let mut all = true;
                    for t in terms.iter().enumerate().filter(|(k, _)| mask & (1 << k) == 0).map(|(_, t)| t) {
                        match find_ci(container.as_bytes(), t) {
                            Some(pos) => score += if at_boundary(container.as_bytes(), pos) { 20 } else { 5 },
                            None => {
                                all = false;
                                break;
                            }
                        }
                    }
                    if !all {
                        continue;
                    }
                }
                batch.push((score, s));
            }
            batch.sort_by_key(|&(sc, s)| (std::cmp::Reverse(sc), s));
            for (_, s) in batch {
                out.push(s);
                if out.len() >= limit {
                    break 'names;
                }
            }
        }
        out
    }
}
