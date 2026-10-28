//! Per-worker macro table.
//!
//! Everything lives in a few flat vectors so a table can be cloned into a
//! worker's scratch copy (`clone_from` reuses the allocations) at the start of
//! each translation unit. Nothing is shared between threads.

use std::hash::{BuildHasher, Hasher};

use hashbrown::HashTable;
use rustc_hash::FxBuildHasher;

use crate::lexer::{Keyword, TokenKind};

pub(super) const NO_PARAM: u16 = u16::MAX;
pub(super) const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Span {
    pub off: u32,
    pub len: u32,
}

/// One token of a macro replacement list. Text lives in the table arena.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BodyTok {
    pub off: u32,
    pub len: u32,
    pub kind: TokenKind,
    pub keyword: Keyword,
    pub flags: u8,
    /// Index of the parameter this token names, or [`NO_PARAM`].
    pub param: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MacroKind {
    Object,
    Function,
    Line,
    File,
    Counter,
    IncludeLevel,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Macro {
    pub name: Span,
    pub kind: MacroKind,
    pub variadic: bool,
    pub nparams: u16,
    pub body_start: u32,
    pub body_len: u32,
    /// True while its own expansion is being rescanned (self-reference guard).
    pub disabled: bool,
}

const FILTER_WORDS: usize = 1024;

/// How to undo one index change.
#[derive(Clone, Copy)]
enum Undo {
    /// A definition was added; `prev` is the live definition it replaced.
    Defined { idx: u32, prev: Option<u32> },
    /// The live definition `idx` was removed.
    Undefined { idx: u32 },
}

/// Lets a table that was reset from a sealed base return to it by undoing
/// only this TU's changes, instead of copying the whole base again.
#[derive(Default)]
struct Lineage {
    /// Nonzero once this table is a finished base that others reset from;
    /// it must not change afterwards.
    seal: u64,
    /// Seal of the base this table currently derives from (0 = none).
    synced: u64,
    /// Index changes made since `synced`; only recorded while `synced != 0`.
    log: Vec<Undo>,
}

impl Clone for Lineage {
    // A copy is a different table: neither sealed nor derived from a base.
    fn clone(&self) -> Self {
        Self::default()
    }
}

static NEXT_SEAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct MacroTable {
    /// Starts with `"01"` so the digits can be referenced without allocating.
    pub arena: String,
    pub defs: Vec<Macro>,
    pub body: Vec<BodyTok>,
    index: HashTable<u32>,
    /// 64Ki-bit Bloom-ish prefilter: almost all identifiers are not macros,
    /// and this keeps that check inside L1.
    filter: [u64; FILTER_WORDS],
    lineage: Lineage,
}

#[inline]
pub(super) fn hash_name(s: &str) -> u64 {
    let mut h = FxBuildHasher.build_hasher();
    h.write(s.as_bytes());
    h.finish()
}

#[inline]
fn filter_pos(h: u64) -> (usize, u64) {
    let bit = (h >> 20) as usize & (FILTER_WORDS * 64 - 1);
    (bit >> 6, 1u64 << (bit & 63))
}

impl MacroTable {
    pub fn heap_bytes(&self) -> usize {
        self.arena.capacity() + self.defs.capacity() * std::mem::size_of::<Macro>() + self.body.capacity() * std::mem::size_of::<BodyTok>() + self.index.capacity() * 5 + FILTER_WORDS * 8
    }

    pub fn new() -> Self {
        Self {
            arena: String::from("01"),
            defs: Vec::new(),
            body: Vec::new(),
            index: HashTable::new(),
            filter: [0; FILTER_WORDS],
            lineage: Lineage::default(),
        }
    }

    /// Marks this table as a finished base (see [`reset_from`](Self::reset_from)).
    /// Cloning it yields an unsealed copy.
    pub fn seal(&mut self) {
        self.lineage.seal = NEXT_SEAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    #[inline]
    pub fn text(&self, s: Span) -> &str {
        &self.arena[s.off as usize..(s.off + s.len) as usize]
    }

    pub fn intern(&mut self, s: &str) -> Span {
        let off = self.arena.len() as u32;
        self.arena.push_str(s);
        Span { off, len: s.len() as u32 }
    }

    /// Index of the live macro called `name`, if any.
    #[inline]
    pub fn lookup(&self, name: &str) -> Option<u32> {
        let h = hash_name(name);
        let (w, m) = filter_pos(h);
        if self.filter[w] & m == 0 {
            return None;
        }
        self.index.find(h, |&i| self.text(self.defs[i as usize].name) == name).copied()
    }

    /// Defines (or redefines) `m`; its name text must already be interned.
    pub fn define(&mut self, m: Macro) {
        let idx = self.defs.len() as u32;
        let name_span = m.name;
        self.defs.push(m);
        let name = &self.arena[name_span.off as usize..(name_span.off + name_span.len) as usize];
        let h = hash_name(name);
        let (w, bit) = filter_pos(h);
        self.filter[w] |= bit;
        let arena = &self.arena;
        let defs = &self.defs;
        let log = self.lineage.synced != 0;
        if let Some(slot) = self.index.find_mut(h, |&i| {
            let s = defs[i as usize].name;
            &arena[s.off as usize..(s.off + s.len) as usize] == name
        }) {
            if log {
                self.lineage.log.push(Undo::Defined { idx, prev: Some(*slot) });
            }
            *slot = idx;
        } else {
            if log {
                self.lineage.log.push(Undo::Defined { idx, prev: None });
            }
            self.index.insert_unique(h, idx, |&i| {
                let s = defs[i as usize].name;
                hash_name(&arena[s.off as usize..(s.off + s.len) as usize])
            });
        }
    }

    /// Removes a macro; returns whether it existed.
    pub fn undef(&mut self, name: &str) -> bool {
        let h = hash_name(name);
        let (w, m) = filter_pos(h);
        if self.filter[w] & m == 0 {
            return false;
        }
        let arena = &self.arena;
        let defs = &self.defs;
        let log = self.lineage.synced != 0;
        match self.index.find_entry(h, |&i| {
            let s = defs[i as usize].name;
            &arena[s.off as usize..(s.off + s.len) as usize] == name
        }) {
            Ok(e) => {
                if log {
                    self.lineage.log.push(Undo::Undefined { idx: *e.get() });
                }
                e.remove();
                true
            }
            Err(_) => false,
        }
    }

    /// Puts a previously live definition back (for `#pragma pop_macro`).
    pub fn restore(&mut self, def_idx: u32) {
        let m = self.defs[def_idx as usize];
        self.define(m);
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Makes `self` equal to `base`, reusing allocations. If `self` already
    /// derives from this sealed `base`, only the changes made since are
    /// undone, which is far cheaper than copying a large base (a table with
    /// the Windows headers or STL pre-included) for every TU.
    pub fn reset_from(&mut self, base: &MacroTable) {
        if base.lineage.seal != 0 && self.lineage.synced == base.lineage.seal {
            while let Some(u) = self.lineage.log.pop() {
                match u {
                    Undo::Defined { idx, prev } => {
                        let h = hash_name(self.text(self.defs[idx as usize].name));
                        match (self.index.find_entry(h, |&i| i == idx), prev) {
                            (Ok(mut e), Some(p)) => *e.get_mut() = p,
                            (Ok(e), None) => {
                                e.remove();
                            }
                            (Err(_), _) => {}
                        }
                    }
                    Undo::Undefined { idx } => {
                        let h = hash_name(self.text(self.defs[idx as usize].name));
                        let (arena, defs) = (&self.arena, &self.defs);
                        self.index.insert_unique(h, idx, |&i| {
                            let s = defs[i as usize].name;
                            hash_name(&arena[s.off as usize..(s.off + s.len) as usize])
                        });
                    }
                }
            }
            self.arena.truncate(base.arena.len());
            self.defs.truncate(base.defs.len());
            self.body.truncate(base.body.len());
            self.filter = base.filter;
            return;
        }
        self.arena.clear();
        self.arena.push_str(&base.arena);
        self.defs.clear();
        self.defs.extend_from_slice(&base.defs);
        self.body.clear();
        self.body.extend_from_slice(&base.body);
        self.index.clone_from(&base.index);
        self.filter = base.filter;
        self.lineage.log.clear();
        self.lineage.synced = base.lineage.seal;
    }
}
