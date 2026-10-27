//! Flat symbol database: contiguous vectors, typed IDs, CSR side indexes.
//!
//! Symbols are 32-byte `Copy` records in one `IndexVec`; relations (parent,
//! bases, virtual family) are IDs rather than pointers. Side indexes
//! ([`SymbolDb::rebuild_indexes`]) are O(n) rebuilds and are stale after any
//! insertion until rebuilt; Phase 5 will replace the global rebuild with
//! per-file shard patching.

use crate::csr::Csr;
use crate::ids::{Idx, IndexVec};
use crate::interner::Interner;
use std::fmt;
use std::ops::{BitOr, BitOrAssign};

crate::define_id!(
    /// Index into [`SymbolDb`]'s symbol array.
    pub SymbolId
);
crate::define_id!(
    /// Interned identifier text.
    pub NameId
);
crate::define_id!(
    /// Interned file path.
    pub FileId
);
crate::define_id!(
    /// A group of virtual methods that override one another (Phase 4).
    pub FamilyId
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SymbolKind {
    Namespace,
    Class,
    Struct,
    Union,
    Enum,
    EnumConstant,
    Function,
    Method,
    Constructor,
    Destructor,
    Field,
    Variable,
    TypeAlias,
    Macro,
}

impl SymbolKind {
    pub const ALL: [Self; 14] = [
        Self::Namespace,
        Self::Class,
        Self::Struct,
        Self::Union,
        Self::Enum,
        Self::EnumConstant,
        Self::Function,
        Self::Method,
        Self::Constructor,
        Self::Destructor,
        Self::Field,
        Self::Variable,
        Self::TypeAlias,
        Self::Macro,
    ];

    pub fn from_u8(v: u8) -> Option<Self> {
        Self::ALL.get(v as usize).copied()
    }

    /// Class-like types that can have bases and members.
    pub fn is_record(self) -> bool {
        matches!(self, Self::Class | Self::Struct | Self::Union)
    }

    pub fn is_type(self) -> bool {
        self.is_record() || matches!(self, Self::Enum | Self::TypeAlias)
    }

    /// Kinds that contain child symbols.
    pub fn is_scope(self) -> bool {
        self.is_record() || matches!(self, Self::Namespace | Self::Enum)
    }

    pub fn is_callable(self) -> bool {
        matches!(self, Self::Function | Self::Method | Self::Constructor | Self::Destructor)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum Access {
    #[default]
    Public,
    Protected,
    Private,
}

impl Access {
    pub fn from_u8(v: u8) -> Option<Self> {
        [Self::Public, Self::Protected, Self::Private].get(v as usize).copied()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SymbolFlags(u16);

impl SymbolFlags {
    pub const EMPTY: Self = Self(0);
    pub const VIRTUAL: Self = Self(1 << 0);
    pub const OVERRIDE: Self = Self(1 << 1);
    pub const FINAL: Self = Self(1 << 2);
    pub const PURE: Self = Self(1 << 3);
    pub const STATIC: Self = Self(1 << 4);
    pub const CONST: Self = Self(1 << 5);
    /// Has a body / full definition (as opposed to a forward declaration).
    pub const DEFINITION: Self = Self(1 << 6);
    pub const INLINE: Self = Self(1 << 7);
    pub const EXPLICIT: Self = Self(1 << 8);
    pub const TEMPLATE: Self = Self(1 << 9);
    pub const DELETED: Self = Self(1 << 10);
    pub const DEFAULTED: Self = Self(1 << 11);
    /// Tombstone: the symbol's file was re-indexed. Never persisted.
    pub const SCOPED: Self = Self(1 << 12);
    /// Tombstone: the symbol's file was re-indexed. Never persisted.
    pub const DEAD: Self = Self(1 << 13);

    const NAMES: [(Self, &'static str); 14] = [
        (Self::VIRTUAL, "VIRTUAL"),
        (Self::OVERRIDE, "OVERRIDE"),
        (Self::FINAL, "FINAL"),
        (Self::PURE, "PURE"),
        (Self::STATIC, "STATIC"),
        (Self::CONST, "CONST"),
        (Self::DEFINITION, "DEFINITION"),
        (Self::INLINE, "INLINE"),
        (Self::EXPLICIT, "EXPLICIT"),
        (Self::TEMPLATE, "TEMPLATE"),
        (Self::DELETED, "DELETED"),
        (Self::DEFAULTED, "DEFAULTED"),
        (Self::SCOPED, "SCOPED"),
        (Self::DEAD, "DEAD"),
    ];

    #[inline]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Unknown bits are dropped.
    pub const fn from_bits_truncate(bits: u16) -> Self {
        Self(bits & ((1 << 13) - 1))
    }

    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[inline]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    #[inline]
    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    #[inline]
    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    /// Takes part in virtual dispatch (declared `virtual` or `override`).
    #[inline]
    pub const fn is_virtual_like(self) -> bool {
        self.intersects(Self(Self::VIRTUAL.0 | Self::OVERRIDE.0))
    }
}

impl BitOr for SymbolFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for SymbolFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl fmt::Debug for SymbolFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (flag, name) in Self::NAMES {
            if self.contains(flag) {
                if !first {
                    f.write_str(" | ")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        if first {
            f.write_str("EMPTY")?;
        }
        Ok(())
    }
}

/// One declaration. `name_offset` is where the identifier starts in `file`; its
/// length is the interned name's length. `extent_*` covers the whole declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub name: NameId,
    /// Enclosing scope. Invariant: always an earlier symbol (`parent < self`),
    /// so parent chains are acyclic; the cache decoder enforces this.
    pub parent: Option<SymbolId>,
    pub family: Option<FamilyId>,
    pub file: FileId,
    pub name_offset: u32,
    pub extent_start: u32,
    pub extent_end: u32,
    pub kind: SymbolKind,
    pub access: Access,
    pub flags: SymbolFlags,
}

const _: () = assert!(size_of::<Symbol>() == 32);

impl Symbol {
    /// A symbol whose extent is just its name; refine with the `with_*` methods.
    pub fn new(name: NameId, kind: SymbolKind, file: FileId, name_offset: u32) -> Self {
        Self {
            name,
            parent: None,
            family: None,
            file,
            name_offset,
            extent_start: name_offset,
            extent_end: name_offset,
            kind,
            access: Access::Public,
            flags: SymbolFlags::EMPTY,
        }
    }

    pub fn with_parent(mut self, parent: Option<SymbolId>) -> Self {
        self.parent = parent;
        self
    }

    pub fn with_extent(mut self, start: u32, end: u32) -> Self {
        self.extent_start = start;
        self.extent_end = end;
        self
    }

    pub fn with_flags(mut self, flags: SymbolFlags) -> Self {
        self.flags = flags;
        self
    }

    pub fn with_access(mut self, access: Access) -> Self {
        self.access = access;
        self
    }
}

/// Cheap overload discriminator for a callable: parameter count plus a hash of
/// the parameter types (names and default values ignored, `const` on the
/// method included). Equal hashes are treated as the same overload; equal
/// arity with different hashes is a weaker match (typedef spellings differ).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signature {
    pub symbol: SymbolId,
    pub arity: u16,
    pub hash: u32,
}

/// `derived : [virtual] access base`. `base` is filled in by name resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BaseEdge {
    pub derived: SymbolId,
    pub base_name: NameId,
    pub base: Option<SymbolId>,
    pub access: Access,
    pub is_virtual: bool,
}

/// Interned file paths plus per-file content hashes (for shard-cache validation).
#[derive(Default)]
pub struct FileTable {
    paths: Interner<FileId>,
    hashes: IndexVec<FileId, u64>,
}

impl FileTable {
    pub fn intern(&mut self, path: &str) -> FileId {
        let (id, new) = self.paths.intern_new(path);
        if new {
            self.hashes.push(0);
        }
        id
    }

    pub fn lookup(&self, path: &str) -> Option<FileId> {
        self.paths.lookup(path)
    }

    pub fn path(&self, id: FileId) -> &str {
        self.paths.get(id)
    }

    pub fn content_hash(&self, id: FileId) -> u64 {
        self.hashes[id]
    }

    pub fn set_content_hash(&mut self, id: FileId, hash: u64) {
        self.hashes[id] = hash;
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn ids(&self) -> impl ExactSizeIterator<Item = FileId> + Clone {
        self.hashes.ids()
    }
}

#[derive(Default)]
pub struct SymbolDb {
    pub names: Interner<NameId>,
    pub files: FileTable,
    symbols: IndexVec<SymbolId, Symbol>,
    bases: Vec<BaseEdge>,
    /// `(definition, qualifier)` for `void A::B::f() {}`-style definitions;
    /// the qualifier is the interned text `A::B`.
    out_of_line: Vec<(SymbolId, NameId)>,
    /// Sorted by symbol (callables only).
    sigs: Vec<Signature>,
    by_name: Csr<NameId, SymbolId>,
    children: Csr<SymbolId, SymbolId>,
    indexed_symbols: usize,
}

impl fmt::Debug for SymbolDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SymbolDb")
            .field("symbols", &self.symbols.len())
            .field("names", &self.names.len())
            .field("files", &self.files.len())
            .field("bases", &self.bases.len())
            .finish()
    }
}

impl SymbolDb {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn intern(&mut self, name: &str) -> NameId {
        self.names.intern(name)
    }

    #[inline]
    pub fn add_symbol(&mut self, symbol: Symbol) -> SymbolId {
        self.symbols.push(symbol)
    }

    pub fn add_base(&mut self, edge: BaseEdge) {
        self.bases.push(edge);
    }

    pub fn add_out_of_line(&mut self, symbol: SymbolId, qualifier: NameId) {
        self.out_of_line.push((symbol, qualifier));
    }

    pub fn out_of_line(&self) -> &[(SymbolId, NameId)] {
        &self.out_of_line
    }

    /// Signatures must be added in increasing symbol order.
    pub fn add_signature(&mut self, sig: Signature) {
        debug_assert!(self.sigs.last().is_none_or(|l| l.symbol < sig.symbol));
        self.sigs.push(sig);
    }

    pub fn signatures(&self) -> &[Signature] {
        &self.sigs
    }

    pub fn signature(&self, id: SymbolId) -> Option<Signature> {
        let i = self.sigs.partition_point(|s| s.symbol < id);
        self.sigs.get(i).filter(|s| s.symbol == id).copied()
    }

    pub fn bases_mut(&mut self) -> &mut [BaseEdge] {
        &mut self.bases
    }

    #[inline]
    pub fn symbol(&self, id: SymbolId) -> &Symbol {
        &self.symbols[id]
    }

    #[inline]
    pub fn symbol_mut(&mut self, id: SymbolId) -> &mut Symbol {
        &mut self.symbols[id]
    }

    pub fn symbols(&self) -> &IndexVec<SymbolId, Symbol> {
        &self.symbols
    }

    pub fn bases(&self) -> &[BaseEdge] {
        &self.bases
    }

    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    #[inline]
    pub fn name(&self, id: SymbolId) -> &str {
        self.names.get(self.symbols[id].name)
    }

    pub fn file_path(&self, id: SymbolId) -> &str {
        self.files.path(self.symbols[id].file)
    }

    /// Byte range of the symbol's identifier within its file.
    pub fn name_range(&self, id: SymbolId) -> (u32, u32) {
        let s = &self.symbols[id];
        (s.name_offset, s.name_offset + self.names.get(s.name).len() as u32)
    }

    /// Appends `Outer::Inner::name` to `out` (no allocation if `out` has capacity).
    pub fn write_qualified_name(&self, id: SymbolId, out: &mut String) {
        let s = &self.symbols[id];
        if let Some(parent) = s.parent {
            self.write_qualified_name(parent, out);
            out.push_str("::");
        }
        out.push_str(self.names.get(s.name));
    }

    pub fn qualified_name(&self, id: SymbolId) -> String {
        let mut out = String::new();
        self.write_qualified_name(id, &mut out);
        out
    }

    /// Appends everything from `shard` (typically built privately by one worker
    /// thread, so no locks are needed while parsing). Names and files are
    /// re-interned once per unique string; symbols are remapped by a constant
    /// offset. Virtual families are shard-local and are dropped; Phase 4
    /// computes them globally. Returns the ID offset applied to the shard's
    /// symbols. Indexes become stale.
    pub fn merge(&mut self, shard: &SymbolDb) -> usize {
        let name_map: Vec<NameId> = shard.names.iter().map(|(_, s)| self.names.intern(s)).collect();
        let file_map: Vec<FileId> = shard
            .files
            .ids()
            .map(|f| {
                let id = self.files.intern(shard.files.path(f));
                self.files.set_content_hash(id, shard.files.content_hash(f));
                id
            })
            .collect();
        let base = self.symbols.len();
        let shift = |id: SymbolId| SymbolId::from_usize(id.index() + base);
        self.symbols.reserve(shard.symbols.len());
        for s in shard.symbols.iter() {
            self.symbols.push(Symbol {
                name: name_map[s.name.index()],
                parent: s.parent.map(shift),
                family: None,
                file: file_map[s.file.index()],
                ..*s
            });
        }
        self.bases.extend(shard.bases.iter().map(|e| BaseEdge {
            derived: shift(e.derived),
            base_name: name_map[e.base_name.index()],
            base: e.base.map(shift),
            ..*e
        }));
        self.out_of_line.extend(shard.out_of_line.iter().map(|&(s, q)| (shift(s), name_map[q.index()])));
        self.sigs.extend(shard.sigs.iter().map(|s| Signature { symbol: shift(s.symbol), ..*s }));
        base
    }

    /// Fast path for edits that only move things: when `shard` declares exactly
    /// the symbols now at `start..end` (same names, kinds, nesting, access,
    /// flags, bases, out-of-line qualifiers and signatures), only source
    /// offsets are updated, in place, and the indexes stay valid. Returns
    /// `false`, changing nothing, when anything else differs.
    pub fn patch_same_shape(&mut self, start: usize, end: usize, shard: &SymbolDb) -> bool {
        if shard.symbols.len() != end - start || end > self.symbols.len() {
            return false;
        }
        let rel = |s: SymbolId| s.index().checked_sub(start).filter(|&r| r < end - start);
        for (i, new) in shard.symbols.iter().enumerate() {
            let id = SymbolId::from_usize(start + i);
            let old = &self.symbols[id];
            if old.kind != new.kind
                || old.access != new.access
                || old.flags != new.flags
                || old.parent.map(|p| p.index().wrapping_sub(start)) != new.parent.map(SymbolId::index)
                || self.name(id) != shard.name(SymbolId::from_usize(i))
            {
                return false;
            }
        }
        let same_sigs = {
            let lo = self.sigs.partition_point(|s| s.symbol.index() < start);
            let hi = self.sigs.partition_point(|s| s.symbol.index() < end);
            self.sigs[lo..hi].len() == shard.sigs.len()
                && self.sigs[lo..hi].iter().zip(&shard.sigs).all(|(o, n)| rel(o.symbol) == Some(n.symbol.index()) && o.arity == n.arity && o.hash == n.hash)
        };
        if !same_sigs {
            return false;
        }
        let mut old_bases = self.bases.iter().filter(|e| rel(e.derived).is_some());
        let same_bases = shard.bases.iter().all(|n| {
            old_bases.next().is_some_and(|o| {
                rel(o.derived) == Some(n.derived.index())
                    && o.access == n.access
                    && o.is_virtual == n.is_virtual
                    && self.names.get(o.base_name) == shard.names.get(n.base_name)
            })
        }) && old_bases.next().is_none();
        if !same_bases {
            return false;
        }
        let mut old_ool = self.out_of_line.iter().filter(|&&(s, _)| rel(s).is_some());
        let same_ool = shard.out_of_line.iter().all(|&(s, q)| {
            old_ool.next().is_some_and(|&(os, oq)| rel(os) == Some(s.index()) && self.names.get(oq) == shard.names.get(q))
        }) && old_ool.next().is_none();
        if !same_ool {
            return false;
        }
        for (i, new) in shard.symbols.iter().enumerate() {
            let s = &mut self.symbols[SymbolId::from_usize(start + i)];
            s.name_offset = new.name_offset;
            s.extent_start = new.extent_start;
            s.extent_end = new.extent_end;
        }
        true
    }

    /// Tombstones `start..end` and drops their bases, out-of-line records and
    /// signatures. Indexes become stale (the caller rebuilds them).
    pub fn kill_range(&mut self, start: usize, end: usize) {
        for i in start..end {
            self.symbols[SymbolId::from_usize(i)].flags.insert(SymbolFlags::DEAD);
        }
        let dead = |s: SymbolId| (start..end).contains(&s.index());
        self.bases.retain(|e| !dead(e.derived));
        self.out_of_line.retain(|&(s, _)| !dead(s));
        self.sigs.retain(|s| !dead(s.symbol));
        self.indexed_symbols = usize::MAX;
    }

    pub fn is_dead(&self, id: SymbolId) -> bool {
        self.symbols[id].flags.contains(SymbolFlags::DEAD)
    }

    /// Number of tombstoned symbols.
    pub fn dead_count(&self) -> usize {
        self.symbols.iter().filter(|s| s.flags.contains(SymbolFlags::DEAD)).count()
    }

    /// Drops tombstoned symbols and renumbers the rest. Returns a prefix map
    /// of length `old_len + 1`: for any old index `i`, `map[i]` is the new
    /// index of the first live symbol at or after `i` (exact for live ones).
    /// Families are cleared and indexes become stale.
    pub fn compact(&mut self) -> Vec<u32> {
        let live = |s: &Symbol| !s.flags.contains(SymbolFlags::DEAD);
        let mut prefix = Vec::with_capacity(self.symbols.len() + 1);
        let mut n = 0u32;
        for s in self.symbols.iter() {
            prefix.push(n);
            n += live(s) as u32;
        }
        prefix.push(n);
        let map = |id: SymbolId| SymbolId::from_usize(prefix[id.index()] as usize);
        let mut out = IndexVec::default();
        for s in self.symbols.iter().filter(|s| live(s)) {
            out.push(Symbol { parent: s.parent.map(map), family: None, ..*s });
        }
        self.symbols = out;
        for e in &mut self.bases {
            e.derived = map(e.derived);
            e.base = None;
        }
        for (s, _) in &mut self.out_of_line {
            *s = map(*s);
        }
        for s in &mut self.sigs {
            s.symbol = map(s.symbol);
        }
        self.indexed_symbols = usize::MAX;
        prefix
    }

    /// Rebuilds the name and children indexes in O(symbols).
    pub fn rebuild_indexes(&mut self) {
        let syms = &self.symbols;
        let live = |s: &Symbol| !s.flags.contains(SymbolFlags::DEAD);
        self.by_name = Csr::build(
            self.names.len(),
            syms.iter_enumerated().filter(|(_, s)| live(s)).map(|(id, s)| (s.name, id)),
        );
        self.children = Csr::build(
            syms.len(),
            syms.iter_enumerated().filter(|(_, s)| live(s)).filter_map(|(id, s)| s.parent.map(|p| (p, id))),
        );
        self.indexed_symbols = syms.len();
    }

    /// False if symbols were added since the last [`Self::rebuild_indexes`].
    pub fn indexes_fresh(&self) -> bool {
        self.indexed_symbols == self.symbols.len()
    }

    /// All symbols named `name`, in insertion order. Needs fresh indexes.
    pub fn lookup(&self, name: &str) -> &[SymbolId] {
        debug_assert!(self.indexes_fresh(), "stale indexes");
        match self.names.lookup(name) {
            Some(n) => self.by_name.get(n),
            None => &[],
        }
    }

    pub fn lookup_id(&self, name: NameId) -> &[SymbolId] {
        debug_assert!(self.indexes_fresh(), "stale indexes");
        self.by_name.get(name)
    }

    /// Direct children of a scope, in insertion order. Needs fresh indexes.
    pub fn children(&self, id: SymbolId) -> &[SymbolId] {
        debug_assert!(self.indexes_fresh(), "stale indexes");
        self.children.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        db: SymbolDb,
        file: FileId,
    }

    impl Fixture {
        fn new() -> Self {
            let mut db = SymbolDb::new();
            let file = db.files.intern("src/a.h");
            Self { db, file }
        }

        fn add(&mut self, name: &str, kind: SymbolKind, parent: Option<SymbolId>) -> SymbolId {
            let n = self.db.intern(name);
            let off = self.db.len() as u32 * 10;
            self.db.add_symbol(Symbol::new(n, kind, self.file, off).with_parent(parent))
        }
    }

    #[test]
    fn symbol_layout() {
        assert_eq!(size_of::<Symbol>(), 32);
        assert_eq!(size_of::<Option<SymbolId>>(), 4);
        assert_eq!(size_of::<Option<FamilyId>>(), 4);
    }

    #[test]
    fn flags() {
        let mut f = SymbolFlags::VIRTUAL | SymbolFlags::CONST;
        assert!(f.contains(SymbolFlags::VIRTUAL) && !f.contains(SymbolFlags::OVERRIDE));
        assert!(f.is_virtual_like());
        f.remove(SymbolFlags::VIRTUAL);
        assert!(!f.is_virtual_like());
        f.insert(SymbolFlags::OVERRIDE);
        assert!(f.is_virtual_like());
        assert_eq!(format!("{f:?}"), "OVERRIDE | CONST");
        assert_eq!(format!("{:?}", SymbolFlags::EMPTY), "EMPTY");
    }

    #[test]
    fn kinds() {
        assert!(SymbolKind::Struct.is_record() && SymbolKind::Struct.is_scope());
        assert!(SymbolKind::Enum.is_type() && !SymbolKind::Enum.is_record());
        assert!(SymbolKind::Namespace.is_scope() && !SymbolKind::Namespace.is_type());
        assert!(SymbolKind::Destructor.is_callable() && !SymbolKind::Field.is_callable());
    }

    #[test]
    fn qualified_names_and_children() {
        let mut fx = Fixture::new();
        let ns = fx.add("engine", SymbolKind::Namespace, None);
        let cls = fx.add("Actor", SymbolKind::Class, Some(ns));
        let m1 = fx.add("Tick", SymbolKind::Method, Some(cls));
        let m2 = fx.add("Update", SymbolKind::Method, Some(cls));
        let other = fx.add("Tick", SymbolKind::Function, Some(ns));
        fx.db.rebuild_indexes();

        assert_eq!(fx.db.qualified_name(m1), "engine::Actor::Tick");
        assert_eq!(fx.db.qualified_name(ns), "engine");
        assert_eq!(fx.db.children(cls), [m1, m2]);
        assert_eq!(fx.db.children(ns), [cls, other]);
        assert!(fx.db.children(m1).is_empty());
        assert_eq!(fx.db.lookup("Tick"), [m1, other]);
        assert!(fx.db.lookup("Missing").is_empty());
        assert_eq!(fx.db.name(m2), "Update");
        assert_eq!(fx.db.file_path(m2), "src/a.h");
    }

    #[test]
    fn write_qualified_name_appends_without_realloc() {
        let mut fx = Fixture::new();
        let ns = fx.add("a", SymbolKind::Namespace, None);
        let c = fx.add("B", SymbolKind::Class, Some(ns));
        let mut buf = String::with_capacity(64);
        let ptr = buf.as_ptr();
        fx.db.write_qualified_name(c, &mut buf);
        assert_eq!(buf, "a::B");
        assert_eq!(buf.as_ptr(), ptr);
    }

    #[test]
    fn name_range_uses_interned_length() {
        let mut fx = Fixture::new();
        let a = fx.add("x", SymbolKind::Variable, None);
        let b = fx.add("longer", SymbolKind::Variable, None);
        assert_eq!(fx.db.name_range(a), (0, 1));
        assert_eq!(fx.db.name_range(b), (10, 16));
    }

    #[test]
    fn merge_remaps_ids_names_and_files() {
        let build = |file: &str, cls: &str, hash: u64| {
            let mut fx = Fixture::new();
            fx.file = fx.db.files.intern(file);
            fx.db.files.set_content_hash(fx.file, hash);
            let c = fx.add(cls, SymbolKind::Class, None);
            let m = fx.add("Tick", SymbolKind::Method, Some(c));
            let bn = fx.db.intern("Base");
            fx.db.add_base(BaseEdge { derived: c, base_name: bn, base: None, access: Access::Private, is_virtual: true });
            (fx.db, m)
        };
        let (a, _) = build("a.h", "A", 11);
        let (b, bm) = build("b.h", "B", 22);

        let mut global = SymbolDb::new();
        assert_eq!(global.merge(&a), 0);
        let base = global.merge(&b);
        assert_eq!(base, 2);
        global.rebuild_indexes();

        let m = SymbolId::from_usize(bm.index() + base);
        assert_eq!(global.qualified_name(m), "B::Tick");
        assert_eq!(global.file_path(m), "b.h");
        assert_eq!(global.files.content_hash(global.symbol(m).file), 22);
        assert_eq!(global.lookup("Tick").len(), 2);
        // Shared names are interned once.
        assert_eq!(global.names.len(), 4); // Fixture "src/a.h" is a file, not a name
        let edge = global.bases()[1];
        assert_eq!(edge.derived.index(), 2);
        assert_eq!(global.names.get(edge.base_name), "Base");
        assert!(edge.is_virtual && edge.access == Access::Private);
        // Shard is untouched and reusable (e.g. kept for the disk cache).
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn staleness_tracking() {
        let mut fx = Fixture::new();
        fx.add("A", SymbolKind::Class, None);
        assert!(!fx.db.indexes_fresh());
        fx.db.rebuild_indexes();
        assert!(fx.db.indexes_fresh());
        fx.add("B", SymbolKind::Class, None);
        assert!(!fx.db.indexes_fresh());
    }

    #[test]
    fn file_table_hashes_and_dedup() {
        let mut t = FileTable::default();
        let a = t.intern("a.cpp");
        let b = t.intern("b.cpp");
        assert_eq!(t.intern("a.cpp"), a);
        assert_eq!(t.len(), 2);
        assert_eq!(t.content_hash(a), 0);
        t.set_content_hash(b, 0xdead_beef);
        assert_eq!(t.content_hash(b), 0xdead_beef);
        assert_eq!(t.lookup("b.cpp"), Some(b));
        assert_eq!(t.path(a), "a.cpp");
    }

    #[test]
    fn base_edges() {
        let mut fx = Fixture::new();
        let base = fx.add("Base", SymbolKind::Class, None);
        let derived = fx.add("Derived", SymbolKind::Class, None);
        let bn = fx.db.intern("Base");
        fx.db.add_base(BaseEdge {
            derived,
            base_name: bn,
            base: Some(base),
            access: Access::Public,
            is_virtual: false,
        });
        assert_eq!(fx.db.bases().len(), 1);
        assert_eq!(fx.db.bases()[0].base, Some(base));
    }
}
