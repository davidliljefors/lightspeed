//! Phase 3: tolerant declaration scanner.
//!
//! Consumes the preprocessor's token stream and records high-level
//! declarations into a [`SymbolDb`]. It never tries to understand function
//! bodies, initializers or annotation macros (`UCLASS(...)`,
//! `GENERATED_BODY()`): those become [`OpaqueTokenBlock`]s (token index
//! ranges) that later phases can expand without touching this parser.
//!
//! Policy: speed and robustness over C++ correctness. Anything that does not
//! look like a declaration is skipped to the next `;` or balanced `}`, and
//! every loop iteration consumes at least one token, so malformed input can
//! neither hang nor panic.

use crate::ids::Idx;
use crate::lexer::{Keyword as Kw, TokenKind as K};
use crate::pp::{PpOutput, PpToken};
use crate::symbols::{Access, BaseEdge, FileId, NameId, Signature, Symbol, SymbolDb, SymbolFlags, SymbolId, SymbolKind};

/// Nesting deeper than this is skipped as opaque to bound recursion.
const MAX_DEPTH: u32 = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpaqueKind {
    /// `{ ... }` of a function, including constructor initializer lists' body.
    Body,
    /// `UCLASS(...)`, `UPROPERTY(...)`, `GENERATED_BODY()`: attaches to the
    /// next declaration (`owner`).
    Annotation,
    /// A statement-like macro call (`DECLARE_LOG_CATEGORY_EXTERN(...);`).
    Macro,
    /// Nesting too deep to scan.
    Skipped,
}

/// A run of tokens the parser deliberately did not interpret: indices into
/// [`PpOutput::tokens`], half-open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpaqueTokenBlock {
    pub kind: OpaqueKind,
    pub start: u32,
    pub end: u32,
    pub owner: Option<SymbolId>,
}

#[derive(Default)]
pub struct ParseOutput {
    pub opaque: Vec<OpaqueTokenBlock>,
    /// Times the parser had to skip input it could not make sense of.
    pub recoveries: u32,
}

impl ParseOutput {
    pub fn clear(&mut self) {
        self.opaque.clear();
        self.recoveries = 0;
    }
}

/// Parses the preprocessed main file `src` into `db` (symbols belong to `file`).
/// Qualified definitions (`void Foo::bar() {}`) are recorded in
/// [`SymbolDb::out_of_line`] for the hierarchy pass to resolve.
pub fn parse(out: &PpOutput, src: &str, db: &mut SymbolDb, file: FileId, res: &mut ParseOutput) {
    res.clear();
    let mut p = Parser {
        out,
        src,
        t: &out.tokens,
        i: 0,
        db,
        file,
        res,
        scopes: vec![Scope { sym: None, record: false, access: Access::Public, name: None }],
        pending: Vec::new(),
        tmpl: false,
        scratch: String::new(),
    };
    p.scope_body(0, true);
    p.add_macros();
}

impl Parser<'_> {
    /// Active `#define`s of the main file become file-scope macro symbols.
    fn add_macros(&mut self) {
        for d in &self.out.defines {
            if d.undef {
                continue;
            }
            let (a, b) = (d.name_loc as usize, (d.name_loc + d.name_len) as usize);
            let Some(name) = self.src.get(a..b) else { continue };
            let id = self.db.intern(name);
            let sym = Symbol::new(id, SymbolKind::Macro, self.file, d.name_loc).with_extent(d.name_loc, d.name_loc + d.name_len);
            self.db.add_symbol(sym);
        }
    }
}

struct Scope {
    sym: Option<SymbolId>,
    record: bool,
    access: Access,
    name: Option<NameId>,
}

struct Parser<'a> {
    out: &'a PpOutput,
    src: &'a str,
    t: &'a [PpToken],
    i: usize,
    db: &'a mut SymbolDb,
    file: FileId,
    res: &'a mut ParseOutput,
    scopes: Vec<Scope>,
    /// Indices into `res.opaque` of annotations waiting for their declaration.
    pending: Vec<u32>,
    tmpl: bool,
    scratch: String,
}

fn is_all_caps(s: &str) -> bool {
    s.len() >= 2 && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') && s.bytes().any(|b| b.is_ascii_uppercase())
}

impl<'a> Parser<'a> {
    #[inline]
    fn k(&self, i: usize) -> Option<K> {
        self.t.get(i).map(|t| t.kind)
    }

    #[inline]
    fn kw(&self, i: usize) -> Kw {
        self.t.get(i).map_or(Kw::None, |t| if t.kind == K::Keyword { t.keyword } else { Kw::None })
    }

    #[inline]
    fn txt(&self, i: usize) -> &'a str {
        self.out.text(&self.t[i], self.src)
    }

    /// `i` is an opener; returns the index just past its match (or the end).
    fn skip_balanced(&self, i: usize) -> usize {
        let mut depth = 0i32;
        let mut j = i;
        while j < self.t.len() {
            match self.t[j].kind {
                K::LParen | K::LBracket | K::LBrace => depth += 1,
                K::RParen | K::RBracket | K::RBrace => {
                    depth -= 1;
                    if depth <= 0 {
                        return j + 1;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        j
    }

    /// `i` is a `<`; returns just past the matching `>`. Gives up at `;` or an
    /// unmatched `}` (a comparison, not a template list).
    fn skip_angles(&self, i: usize) -> usize {
        let mut depth = 0i32;
        let mut j = i;
        while j < self.t.len() {
            match self.t[j].kind {
                K::Lt => depth += 1,
                K::Gt => {
                    depth -= 1;
                    if depth == 0 {
                        return j + 1;
                    }
                }
                K::LParen | K::LBracket | K::LBrace => {
                    j = self.skip_balanced(j);
                    continue;
                }
                K::Semi | K::RBrace => return j,
                _ => {}
            }
            j += 1;
        }
        j
    }

    /// From `i`, the index of the next top-level `;` (not consumed), `,` or `}`.
    /// Template argument lists (`is_same_v<A, B>`) are skipped whole so their
    /// commas don't look like more declarators.
    fn skip_initializer(&self, mut j: usize) -> usize {
        while j < self.t.len() {
            match self.t[j].kind {
                K::Semi | K::Comma | K::RBrace => return j,
                K::LParen | K::LBracket | K::LBrace => j = self.skip_balanced(j),
                K::Lt if j > 0 && self.t[j - 1].kind == K::Ident => {
                    let e = self.skip_angles(j);
                    // Unclosed means a comparison, not a template list.
                    j = if e > j + 1 && self.t[e - 1].kind == K::Gt { e } else { j + 1 };
                }
                _ => j += 1,
            }
        }
        j
    }

    /// Skips a malformed declaration: through the next `;`, or a balanced block.
    fn recover(&mut self, mut j: usize) -> usize {
        self.res.recoveries += 1;
        while j < self.t.len() {
            match self.t[j].kind {
                K::Semi => return j + 1,
                K::RBrace => return j,
                K::LBrace => return self.skip_balanced(j),
                K::LParen | K::LBracket => j = self.skip_balanced(j),
                _ => j += 1,
            }
        }
        j
    }

    fn opaque(&mut self, kind: OpaqueKind, start: usize, end: usize, owner: Option<SymbolId>) -> u32 {
        self.res.opaque.push(OpaqueTokenBlock { kind, start: start as u32, end: end as u32, owner });
        self.res.opaque.len() as u32 - 1
    }

    fn extent(&self, start: usize, end: usize) -> (u32, u32) {
        let s = self.t[start.min(self.t.len() - 1)].loc;
        let last = &self.t[end.clamp(1, self.t.len()) - 1];
        (s, (last.loc + last.len).max(s))
    }

    fn add(&mut self, kind: SymbolKind, name: NameId, name_tok: usize, start: usize, end: usize, mut flags: SymbolFlags) -> SymbolId {
        let scope = self.scopes.last().expect("scope stack is never empty");
        let (parent, access) = (scope.sym, scope.access);
        if std::mem::take(&mut self.tmpl) {
            flags.insert(SymbolFlags::TEMPLATE);
        }
        let (a, b) = self.extent(start, end);
        let sym = Symbol::new(name, kind, self.file, self.t[name_tok].loc)
            .with_parent(parent)
            .with_access(access)
            .with_flags(flags)
            .with_extent(a, b);
        let id = self.db.add_symbol(sym);
        for idx in self.pending.drain(..) {
            self.res.opaque[idx as usize].owner = Some(id);
        }
        id
    }

    fn set_extent_end(&mut self, id: SymbolId, end_tok: usize) {
        let (_, e) = self.extent(end_tok, end_tok + 1);
        let s = self.db.symbol_mut(id);
        s.extent_end = e.max(s.extent_start);
    }

    // ---------------------------------------------------------------- scopes

    /// Parses declarations until the scope's closing `}` (not consumed) or EOF.
    fn scope_body(&mut self, depth: u32, top: bool) {
        while self.i < self.t.len() {
            if self.t[self.i].kind == K::RBrace {
                if top {
                    self.i += 1;
                    self.res.recoveries += 1;
                    continue;
                }
                break;
            }
            let before = self.i;
            self.decl(depth);
            if self.i <= before {
                self.i = before + 1;
                self.res.recoveries += 1;
            }
        }
        self.pending.clear();
    }

    /// Runs `scope_body` for a `{`-delimited body at `self.i` (the `{`).
    fn braced_scope(&mut self, depth: u32, scope: Scope) {
        if depth >= MAX_DEPTH {
            let end = self.skip_balanced(self.i);
            self.opaque(OpaqueKind::Skipped, self.i, end, None);
            self.i = end;
            return;
        }
        self.i += 1;
        self.scopes.push(scope);
        self.scope_body(depth + 1, false);
        self.scopes.pop();
        if self.k(self.i) == Some(K::RBrace) {
            self.i += 1;
        }
    }

    fn decl(&mut self, depth: u32) {
        let i = self.i;
        match self.k(i) {
            Some(K::Semi) => {
                self.i += 1;
                self.tmpl = false;
            }
            Some(K::Keyword) => match self.t[i].keyword {
                Kw::Namespace => self.namespace(depth),
                Kw::Inline if self.kw(i + 1) == Kw::Namespace => {
                    self.i += 1;
                    self.namespace(depth);
                }
                Kw::Extern if self.k(i + 1) == Some(K::String) => {
                    if self.k(i + 2) == Some(K::LBrace) {
                        self.i += 2;
                        self.braced_scope(depth, Scope { sym: self.cur_sym(), record: false, access: Access::Public, name: None });
                    } else {
                        self.i += 2;
                    }
                }
                Kw::Template => {
                    if self.k(i + 1) == Some(K::Lt) {
                        self.i = self.skip_angles(i + 1);
                        self.tmpl = true;
                    } else {
                        // Explicit instantiation: `template class X<int>;`
                        self.i = self.recover(i);
                        self.res.recoveries -= 1;
                    }
                }
                Kw::Using => self.using_decl(),
                Kw::Typedef => self.typedef_decl(depth),
                Kw::Public | Kw::Private | Kw::Protected if self.k(i + 1) == Some(K::Colon) => {
                    let a = match self.t[i].keyword {
                        Kw::Public => Access::Public,
                        Kw::Protected => Access::Protected,
                        _ => Access::Private,
                    };
                    if let Some(s) = self.scopes.last_mut().filter(|s| s.record) {
                        s.access = a;
                    }
                    self.i += 2;
                }
                Kw::Friend => self.i = self.recover(i + 1),
                Kw::Class | Kw::Struct | Kw::Union => {
                    if !self.record(depth, false) {
                        self.generic(depth);
                    }
                }
                Kw::Enum => {
                    if !self.enum_decl(false) {
                        self.generic(depth);
                    }
                }
                _ => self.generic(depth),
            },
            Some(K::Ident) if self.txt(i) == "static_assert" => {
                let end = if self.k(i + 1) == Some(K::LParen) { self.skip_balanced(i + 1) } else { i + 1 };
                self.i = if self.k(end) == Some(K::Semi) { end + 1 } else { end };
            }
            Some(K::Hash | K::HashHash) => self.i += 1,
            _ => self.generic(depth),
        }
    }

    fn cur_sym(&self) -> Option<SymbolId> {
        self.scopes.last().and_then(|s| s.sym)
    }

    fn namespace(&mut self, depth: u32) {
        let start = self.i;
        let mut j = start + 1;
        let mut names: Vec<usize> = Vec::new();
        loop {
            match self.k(j) {
                Some(K::LBracket) => j = self.skip_balanced(j),
                Some(K::Ident) if self.k(j + 1) == Some(K::LParen) => j = self.skip_balanced(j + 1),
                Some(K::Ident) => {
                    names.push(j);
                    j += 1;
                }
                Some(K::ColonColon) => j += 1,
                Some(K::Keyword) if self.kw(j) == Kw::Inline => j += 1,
                _ => break,
            }
        }
        if self.k(j) != Some(K::LBrace) {
            // `namespace A = B;` or garbage.
            self.i = self.recover(j);
            return;
        }
        let mut pushed = 0;
        let mut last = None;
        for &n in &names {
            let name = self.db.intern(self.txt(n));
            let id = self.add(SymbolKind::Namespace, name, n, start, j + 1, SymbolFlags::DEFINITION);
            self.scopes.push(Scope { sym: Some(id), record: false, access: Access::Public, name: Some(name) });
            pushed += 1;
            last = Some(id);
        }
        self.i = j;
        // Names were pushed as scopes already; the body scope is transparent.
        let sym = self.cur_sym();
        if depth >= MAX_DEPTH {
            let end = self.skip_balanced(self.i);
            self.opaque(OpaqueKind::Skipped, self.i, end, None);
            self.i = end;
        } else {
            self.i += 1;
            self.scopes.push(Scope { sym, record: false, access: Access::Public, name: None });
            self.scope_body(depth + 1, false);
            self.scopes.pop();
            let close = self.i;
            if self.k(close) == Some(K::RBrace) {
                self.i += 1;
                for _ in 0..pushed {
                    if let Some(id) = last {
                        self.set_extent_end(id, close);
                    }
                }
            }
        }
        for _ in 0..pushed {
            self.scopes.pop();
        }
        if let Some(id) = last {
            // Outer segments of `namespace a::b` end where the inner one does.
            let end = self.db.symbol(id).extent_end;
            for k in (0..names.len().saturating_sub(1)).rev() {
                let sid = SymbolId::from_usize(id.index() - (names.len() - 1 - k));
                let s = self.db.symbol_mut(sid);
                s.extent_end = end.max(s.extent_start);
            }
        }
    }

    fn using_decl(&mut self) {
        let start = self.i;
        if self.k(start + 1) == Some(K::Ident) && self.k(start + 2) == Some(K::Eq) {
            let end = self.recover_to_semi(start + 3);
            let name = self.db.intern(self.txt(start + 1));
            self.add(SymbolKind::TypeAlias, name, start + 1, start, end, SymbolFlags::EMPTY);
            self.i = end;
        } else {
            self.tmpl = false;
            self.i = self.recover_to_semi(start + 1);
        }
    }

    /// Index just past the next top-level `;` (or the end of the scope).
    fn recover_to_semi(&self, mut j: usize) -> usize {
        while j < self.t.len() {
            match self.t[j].kind {
                K::Semi => return j + 1,
                K::RBrace => return j,
                K::LParen | K::LBracket | K::LBrace => j = self.skip_balanced(j),
                _ => j += 1,
            }
        }
        j
    }

    fn typedef_decl(&mut self, depth: u32) {
        let start = self.i;
        self.i = start + 1;
        if matches!(self.kw(start + 1), Kw::Class | Kw::Struct | Kw::Union) {
            if self.record(depth, true) {
                return;
            }
        } else if self.kw(start + 1) == Kw::Enum && self.enum_decl(true) {
            return;
        }
        self.i = start;
        let mut name = None;
        let mut j = start + 1;
        while j < self.t.len() {
            match self.t[j].kind {
                K::Semi | K::RBrace => break,
                K::LParen => {
                    if matches!(self.k(j + 1), Some(K::Star | K::Amp)) {
                        let close = self.skip_balanced(j);
                        if let Some(n) = (j + 1..close).rev().find(|&x| self.t[x].kind == K::Ident) {
                            name = Some(n);
                        }
                        j = close;
                        if self.k(j) == Some(K::LParen) {
                            j = self.skip_balanced(j);
                        }
                    } else {
                        j = self.skip_balanced(j);
                    }
                }
                K::LBrace | K::LBracket => j = self.skip_balanced(j),
                K::Lt => j = self.skip_angles(j).max(j + 1),
                K::Ident => {
                    name = Some(j);
                    j += 1;
                }
                _ => j += 1,
            }
        }
        let end = if self.k(j) == Some(K::Semi) { j + 1 } else { j };
        if let Some(n) = name.filter(|&n| n > start + 1) {
            let id = self.db.intern(self.txt(n));
            self.add(SymbolKind::TypeAlias, id, n, start, end, SymbolFlags::EMPTY);
        }
        self.tmpl = false;
        self.i = end.max(start + 1);
    }

    // --------------------------------------------------------------- records

    /// `class`/`struct`/`union` at `self.i`. Returns false (consuming nothing)
    /// when it is an elaborated type in some other declaration.
    fn record(&mut self, depth: u32, typedef: bool) -> bool {
        let kw_i = self.i;
        let kind = match self.t[kw_i].keyword {
            Kw::Class => SymbolKind::Class,
            Kw::Struct => SymbolKind::Struct,
            _ => SymbolKind::Union,
        };
        let mut j = kw_i + 1;
        let mut cand: Option<usize> = None;
        let mut spec = SymbolFlags::EMPTY;
        loop {
            match self.k(j) {
                Some(K::LBracket) => j = self.skip_balanced(j),
                Some(K::Ident) if self.k(j + 1) == Some(K::LParen) => j = self.skip_balanced(j + 1),
                Some(K::Ident) => {
                    cand = Some(j);
                    j += 1;
                }
                Some(K::ColonColon) => j += 1,
                Some(K::Lt) if cand.is_some() => {
                    spec = SymbolFlags::SPECIALIZATION;
                    j = self.skip_angles(j).max(j + 1)
                }
                Some(K::Keyword) if self.kw(j) == Kw::Final && cand.is_some() => j += 1,
                _ => break,
            }
        }
        let default_access = if kind == SymbolKind::Class { Access::Private } else { Access::Public };
        match self.k(j) {
            Some(K::Semi) => {
                let Some(c) = cand else { return false };
                let name = self.db.intern(self.txt(c));
                self.add(kind, name, c, kw_i, j + 1, spec);
                self.i = j + 1;
                true
            }
            Some(K::LBrace | K::Colon) => {
                let sym = cand.map(|c| {
                    let name = self.db.intern(self.txt(c));
                    let mut flags = spec;
                    flags.insert(SymbolFlags::DEFINITION);
                    (self.add(kind, name, c, kw_i, j, flags), name)
                });
                if let Some((id, _)) = sym {
                    if self.k(j) == Some(K::Colon) {
                        j = self.bases(id, j, default_access);
                    }
                } else if self.k(j) == Some(K::Colon) {
                    // Anonymous with bases: skip them.
                    while j < self.t.len() && self.t[j].kind != K::LBrace {
                        j += 1;
                    }
                }
                if self.k(j) != Some(K::LBrace) {
                    self.i = self.recover(j);
                    return true;
                }
                self.i = j;
                let scope = match sym {
                    Some((id, name)) => Scope { sym: Some(id), record: true, access: default_access, name: Some(name) },
                    None => Scope { sym: self.cur_sym(), record: true, access: Access::Public, name: None },
                };
                self.braced_scope(depth, scope);
                let close = self.i.saturating_sub(1);
                if let Some((id, _)) = sym
                    && self.k(close) == Some(K::RBrace)
                {
                    self.set_extent_end(id, close);
                }
                self.declarators_after_body(typedef, kw_i);
                true
            }
            _ => false,
        }
    }

    /// After `}` of a type definition: `... name1, *name2;` (typedef names
    /// become aliases, plain declarators are ignored).
    fn declarators_after_body(&mut self, typedef: bool, start: usize) {
        let end = self.recover_to_semi(self.i);
        if typedef {
            for n in self.i..end {
                if !(self.t[n].kind == K::Ident && self.k(n + 1).is_none_or(|k| matches!(k, K::Semi | K::Comma | K::LBracket))) {
                    continue;
                }
                let id = self.db.intern(self.txt(n));
                self.add(SymbolKind::TypeAlias, id, n, start, end, SymbolFlags::EMPTY);
            }
        }
        self.i = end;
    }

    /// Parses `: public A, virtual B<T>` starting at the `:`; returns the index
    /// of the `{` (or wherever it stopped).
    fn bases(&mut self, derived: SymbolId, colon: usize, default_access: Access) -> usize {
        let mut j = colon + 1;
        let (mut access, mut virt, mut last) = (default_access, false, None::<usize>);
        // First identifier of the current qualified name, and a leading `::`.
        let mut q = (0usize, false);
        loop {
            match self.k(j) {
                None | Some(K::LBrace | K::Semi | K::RBrace) => break,
                Some(K::Comma) => {
                    self.flush_base(derived, &mut last, q, access, virt);
                    q = (0, false);
                    access = default_access;
                    virt = false;
                    j += 1;
                }
                Some(K::ColonColon) => {
                    if last.is_none() && q.0 == 0 {
                        q.1 = true;
                    }
                    j += 1;
                }
                Some(K::Keyword) => {
                    match self.t[j].keyword {
                        Kw::Public => access = Access::Public,
                        Kw::Protected => access = Access::Protected,
                        Kw::Private => access = Access::Private,
                        Kw::Virtual => virt = true,
                        _ => {}
                    }
                    j += 1;
                }
                Some(K::Ident) if self.k(j + 1) == Some(K::LParen) => j = self.skip_balanced(j + 1),
                Some(K::Ident) => {
                    if q.0 == 0 || self.t[j - 1].kind != K::ColonColon {
                        q.0 = j;
                    }
                    last = Some(j);
                    j += 1;
                }
                Some(K::Lt) => j = self.skip_angles(j).max(j + 1),
                _ => j += 1,
            }
        }
        self.flush_base(derived, &mut last, q, access, virt);
        j
    }

    /// `q` is the first identifier of the qualified base name and whether it
    /// starts with `::`. The qualifier (`A::B` for `A::B::Base`, empty for
    /// `::Base`) is recorded so resolution can honour it.
    fn flush_base(&mut self, derived: SymbolId, last: &mut Option<usize>, q: (usize, bool), access: Access, is_virtual: bool) {
        if let Some(l) = last.take() {
            let base_name = self.db.intern(self.txt(l));
            let qual = if q.0 < l {
                let mut s = String::new();
                let mut depth = 0u32;
                for k in q.0..l {
                    match self.t[k].kind {
                        K::Lt => depth += 1,
                        K::Gt => depth = depth.saturating_sub(1),
                        K::Ident if depth == 0 => {
                            if !s.is_empty() {
                                s.push_str("::");
                            }
                            s.push_str(self.txt(k));
                        }
                        _ => {}
                    }
                }
                Some(self.db.intern(&s))
            } else if q.1 {
                Some(self.db.intern(""))
            } else {
                None
            };
            self.db.add_base(BaseEdge { derived, base_name, qual, base: None, access, is_virtual });
        }
    }

    // ----------------------------------------------------------------- enums

    fn enum_decl(&mut self, typedef: bool) -> bool {
        let start = self.i;
        let mut j = start + 1;
        let scoped = matches!(self.kw(j), Kw::Class | Kw::Struct);
        if scoped {
            j += 1;
        }
        let mut cand = None;
        loop {
            match self.k(j) {
                Some(K::LBracket) => j = self.skip_balanced(j),
                Some(K::Ident) if self.k(j + 1) == Some(K::LParen) => j = self.skip_balanced(j + 1),
                Some(K::Ident) => {
                    cand = Some(j);
                    j += 1;
                }
                Some(K::ColonColon) => j += 1,
                _ => break,
            }
        }
        // Underlying type: `: uint8_t`
        let mut k = j;
        if self.k(k) == Some(K::Colon) {
            while !matches!(self.k(k), None | Some(K::LBrace | K::Semi | K::RBrace)) {
                k += 1;
            }
        }
        match self.k(k) {
            Some(K::Semi) => {
                let Some(c) = cand else { return false };
                let name = self.db.intern(self.txt(c));
                self.add(SymbolKind::Enum, name, c, start, k + 1, SymbolFlags::EMPTY);
                self.i = k + 1;
                true
            }
            Some(K::LBrace) => {
                let sym = cand.map(|c| {
                    let name = self.db.intern(self.txt(c));
                    let flags = if scoped { SymbolFlags::DEFINITION | SymbolFlags::SCOPED } else { SymbolFlags::DEFINITION };
                    self.add(SymbolKind::Enum, name, c, start, k, flags)
                });
                let close = self.enum_body(k, sym);
                if let Some(id) = sym
                    && self.k(close) == Some(K::RBrace)
                {
                    self.set_extent_end(id, close);
                }
                self.i = if self.k(close) == Some(K::RBrace) { close + 1 } else { close };
                self.declarators_after_body(typedef, start);
                true
            }
            _ => false,
        }
    }

    /// Parses enumerators; `open` is the `{`. Returns the index of the `}`.
    fn enum_body(&mut self, open: usize, owner: Option<SymbolId>) -> usize {
        let parent = owner.or_else(|| self.cur_sym());
        let access = self.scopes.last().map_or(Access::Public, |s| s.access);
        let mut j = open + 1;
        loop {
            match self.k(j) {
                None | Some(K::RBrace) => return j,
                Some(K::Ident) => {
                    let name = self.db.intern(self.txt(j));
                    let (a, b) = self.extent(j, j + 1);
                    let sym = Symbol::new(name, SymbolKind::EnumConstant, self.file, self.t[j].loc)
                        .with_parent(parent)
                        .with_access(access)
                        .with_extent(a, b);
                    let id = self.db.add_symbol(sym);
                    j += 1;
                    // `= expr`, `UMETA(...)`: skip to the separator.
                    let ann_start = j;
                    while !matches!(self.k(j), None | Some(K::Comma | K::RBrace)) {
                        j = if matches!(self.k(j), Some(K::LParen | K::LBracket | K::LBrace)) { self.skip_balanced(j) } else { j + 1 };
                    }
                    if self.k(ann_start) == Some(K::Ident) && self.k(ann_start + 1) == Some(K::LParen) {
                        self.opaque(OpaqueKind::Annotation, ann_start, self.skip_balanced(ann_start + 1), Some(id));
                    }
                }
                Some(K::Comma) => j += 1,
                _ => {
                    // Unexpected token inside the enum: skip it.
                    self.res.recoveries += 1;
                    j = if matches!(self.k(j), Some(K::LParen | K::LBracket | K::LBrace)) { self.skip_balanced(j) } else { j + 1 };
                }
            }
        }
    }

    // ------------------------------------------------- functions and variables

    fn emit_var(&mut self, nm: usize, chain: usize, start: usize, end: usize, flags: SymbolFlags) {
        let in_record = self.scopes.last().is_some_and(|s| s.record);
        let kind = if in_record && !flags.contains(SymbolFlags::STATIC) { SymbolKind::Field } else { SymbolKind::Variable };
        let name = self.db.intern(self.txt(nm));
        let id = self.add(kind, name, nm, start, end, flags);
        self.note_qualifier(id, chain, nm);
    }

    /// Records `A::B` from tokens `chain..nm` when the declarator is qualified.
    fn note_qualifier(&mut self, id: SymbolId, chain: usize, nm: usize) {
        if chain >= nm {
            return;
        }
        let mut q = std::mem::take(&mut self.scratch);
        q.clear();
        let mut angle = 0i32;
        for k in chain..nm {
            match self.t[k].kind {
                K::Lt => angle += 1,
                K::Gt => angle = (angle - 1).max(0),
                K::Ident if angle == 0 => {
                    if !q.is_empty() {
                        q.push_str("::");
                    }
                    q.push_str(self.txt(k));
                }
                _ => {}
            }
        }
        if !q.is_empty() {
            let qualifier = self.db.intern(&q);
            self.db.add_out_of_line(id, qualifier);
        }
        self.scratch = q;
    }

    fn generic(&mut self, _depth: u32) {
        let n = self.t.len();
        let start = self.i;
        let mut j = start;
        let mut angle = 0u32;
        let mut name: Option<usize> = None;
        let mut chain = start;
        let mut op_end = 0usize;
        let mut flags = SymbolFlags::EMPTY;
        let mut n_idents = 0u32;
        let mut prev_cc = false;

        macro_rules! var {
            ($end:expr) => {
                if let Some(nm) = name.take()
                    && n_idents >= 2
                {
                    self.emit_var(nm, chain, start, $end, flags);
                }
            };
        }

        while j < n {
            let tk = self.t[j];
            let was_cc = std::mem::take(&mut prev_cc);
            match tk.kind {
                K::Semi => {
                    var!(j + 1);
                    j += 1;
                    self.tmpl = false;
                    self.i = j;
                    return;
                }
                K::RBrace => break,
                K::ColonColon | K::Tilde => {
                    prev_cc = was_cc || tk.kind == K::ColonColon;
                    j += 1;
                }
                K::LBrace => {
                    var!(j);
                    j = self.skip_balanced(j);
                }
                K::LBracket => j = self.skip_balanced(j),
                K::Lt => {
                    angle += 1;
                    j += 1;
                }
                K::Gt => {
                    angle = angle.saturating_sub(1);
                    j += 1;
                }
                K::LParen if angle > 0 => j = self.skip_balanced(j),
                K::LParen => {
                    let close = self.skip_balanced(j);
                    let prev = j.checked_sub(1).filter(|&p| p >= start);
                    let prev_text = prev.filter(|&p| self.t[p].kind == K::Ident).map_or("", |p| self.txt(p));
                    if matches!(prev_text, "decltype" | "__declspec" | "alignas" | "__attribute__" | "typeof" | "__typeof__" | "sizeof" | "alignof" | "_Alignas" | "__pragma") {
                        if name == prev {
                            name = None;
                            n_idents = n_idents.saturating_sub(1);
                        }
                        j = close;
                        continue;
                    }
                    if matches!(self.k(j + 1), Some(K::Star | K::Amp | K::AmpAmp)) {
                        // `void (*callback)(int)`
                        if let Some(nm) = (j + 1..close).rev().find(|&x| self.t[x].kind == K::Ident) {
                            name = Some(nm);
                            chain = nm;
                            n_idents += 1;
                        }
                        j = close;
                        if self.k(j) == Some(K::LParen) {
                            j = self.skip_balanced(j);
                        }
                        continue;
                    }
                    let named = name.is_some_and(|nm| if self.t[nm].keyword == Kw::Operator && self.t[nm].kind == K::Keyword { op_end == j } else { nm + 1 == j });
                    let Some(nm) = name.filter(|_| named) else {
                        j = close;
                        continue;
                    };
                    let is_op = self.t[nm].kind == K::Keyword;
                    let nm_txt = if is_op { "" } else { self.txt(nm) };
                    let tilde = nm > start && self.t[nm - 1].kind == K::Tilde;
                    if !is_op && !tilde && is_all_caps(nm_txt) {
                        let nxt = self.k(close);
                        let continues = matches!(nxt, Some(K::Eq | K::Colon | K::LBrace | K::Arrow | K::Amp | K::AmpAmp))
                            || matches!(self.kw(close), Kw::Const | Kw::Noexcept | Kw::Override | Kw::Final | Kw::Volatile | Kw::Requires);
                        if n_idents == 1 && flags == SymbolFlags::EMPTY {
                            if nxt == Some(K::Semi) {
                                self.opaque(OpaqueKind::Macro, start, close + 1, None);
                                self.i = close + 1;
                                self.tmpl = false;
                                return;
                            }
                            if !continues {
                                let idx = self.opaque(OpaqueKind::Annotation, start, close, None);
                                self.pending.push(idx);
                                self.i = close;
                                return;
                            }
                        } else if !continues && nxt != Some(K::Semi) {
                            // Mid-declaration annotation such as `UE_DEPRECATED(...)`.
                            name = None;
                            n_idents = n_idents.saturating_sub(1);
                            j = close;
                            continue;
                        }
                    }
                    self.function(start, nm, chain, op_end, close, flags);
                    return;
                }
                K::Eq if angle == 0 => {
                    var!(j);
                    j = self.skip_initializer(j + 1);
                }
                K::Colon if angle == 0 => {
                    var!(j);
                    j = self.skip_initializer(j + 1);
                }
                K::Comma if angle == 0 => {
                    var!(j);
                    j += 1;
                }
                K::Ident => {
                    if angle == 0 {
                        if !was_cc {
                            n_idents += 1;
                            chain = j;
                        }
                        name = Some(j);
                    }
                    j += 1;
                }
                K::Keyword => match tk.keyword {
                    Kw::Virtual => {
                        flags.insert(SymbolFlags::VIRTUAL);
                        j += 1;
                    }
                    Kw::Static => {
                        flags.insert(SymbolFlags::STATIC);
                        j += 1;
                    }
                    Kw::Inline | Kw::Constexpr | Kw::Consteval => {
                        flags.insert(SymbolFlags::INLINE);
                        j += 1;
                    }
                    Kw::Explicit => {
                        flags.insert(SymbolFlags::EXPLICIT);
                        j += 1;
                    }
                    Kw::Public | Kw::Private | Kw::Protected if self.k(j + 1) == Some(K::Colon) => {
                        // `Q_OBJECT` followed by an access label: nothing to declare.
                        self.i = j;
                        return;
                    }
                    Kw::Operator if angle == 0 => {
                        let mut k = j + 1;
                        if self.k(k) == Some(K::LParen) && self.k(k + 1) == Some(K::RParen) && self.k(k + 2) == Some(K::LParen) {
                            k += 2;
                        } else {
                            while !matches!(self.k(k), None | Some(K::LParen | K::Semi | K::LBrace | K::RBrace)) {
                                k += 1;
                            }
                        }
                        if !was_cc {
                            n_idents += 1;
                            chain = j;
                        }
                        name = Some(j);
                        op_end = k;
                        j = k;
                    }
                    _ => j += 1,
                },
                _ => j += 1,
            }
        }
        var!(j);
        self.tmpl = false;
        self.i = j.max(start + 1).min(n);
        if self.i == start {
            self.i = start + 1;
        }
    }

    /// Arity and type hash of the parameter list `(` at `open` .. `)` before `close`.
    /// Parameter names and default values are dropped so a declaration and its
    /// out-of-line definition agree.
    fn param_signature(&self, open: usize, close: usize, is_const: bool) -> (u16, u32) {
        use std::hash::Hasher;
        let end = close.saturating_sub(1).min(self.t.len());
        let mut h = rustc_hash::FxHasher::default();
        let (mut arity, mut angle, mut depth) = (0u32, 0i32, 0i32);
        let mut seg = open + 1;
        let mut j = open + 1;
        // Hashes tokens `from..to` of one parameter.
        let flush = |this: &Self, from: usize, to: usize, h: &mut rustc_hash::FxHasher| {
            let mut to = to;
            if let Some(e) = (from..to).find(|&k| this.t[k].kind == K::Eq) {
                to = e;
            }
            if to <= from {
                return false;
            }
            let last = to - 1;
            // `Foo x` / `Foo& x`: drop the trailing name, but keep a lone type.
            if this.t[last].kind == K::Ident
                && last > from
                && matches!(this.t[last - 1].kind, K::Ident | K::Amp | K::AmpAmp | K::Star | K::Gt | K::RBracket)
            {
                to = last;
            }
            if to - from == 1 && this.txt(from) == "void" {
                return false;
            }
            for k in from..to {
                h.write(this.txt(k).as_bytes());
                h.write_u8(0);
            }
            h.write_u8(1);
            true
        };
        while j <= end {
            let k = if j < end { self.t[j].kind } else { K::Comma };
            match k {
                K::LParen | K::LBracket | K::LBrace => depth += 1,
                K::RParen | K::RBracket | K::RBrace => depth -= 1,
                K::Lt if depth == 0 => angle += 1,
                K::Gt if depth == 0 => angle = (angle - 1).max(0),
                K::Comma if depth <= 0 && angle == 0 => {
                    if flush(self, seg, j, &mut h) {
                        arity += 1;
                    }
                    seg = j + 1;
                }
                _ => {}
            }
            j += 1;
        }
        h.write_u8(is_const as u8);
        let v = h.finish();
        (arity.min(u16::MAX as u32) as u16, (v ^ (v >> 32)) as u32)
    }

    /// A declarator followed by a parameter list; `close` is just past `)`.
    fn function(&mut self, start: usize, nm: usize, chain: usize, op_end: usize, close: usize, mut flags: SymbolFlags) {
        let n = self.t.len();
        let mut p = close;
        loop {
            match self.k(p) {
                Some(K::Keyword) => match self.t[p].keyword {
                    Kw::Const => {
                        flags.insert(SymbolFlags::CONST);
                        p += 1;
                    }
                    Kw::Volatile => p += 1,
                    Kw::Noexcept => {
                        p += 1;
                        if self.k(p) == Some(K::LParen) {
                            p = self.skip_balanced(p);
                        }
                    }
                    Kw::Override => {
                        flags.insert(SymbolFlags::OVERRIDE);
                        p += 1;
                    }
                    Kw::Final => {
                        flags.insert(SymbolFlags::FINAL);
                        p += 1;
                    }
                    Kw::Requires => {
                        p += 1;
                        while !matches!(self.k(p), None | Some(K::LBrace | K::Semi | K::Eq)) {
                            p = if self.k(p) == Some(K::LParen) { self.skip_balanced(p) } else { p + 1 };
                        }
                    }
                    _ => break,
                },
                Some(K::Amp | K::AmpAmp) => p += 1,
                Some(K::Arrow) => {
                    p += 1;
                    while !matches!(self.k(p), None | Some(K::LBrace | K::Semi | K::Eq | K::RBrace)) {
                        p = if matches!(self.k(p), Some(K::LParen | K::LBracket)) { self.skip_balanced(p) } else { p + 1 };
                    }
                }
                Some(K::LBracket) => p = self.skip_balanced(p),
                Some(K::Ident) => {
                    // `throw()`, `__attribute__((...))`, trailing macros like `PURE_VIRTUAL(...)`.
                    p = if self.k(p + 1) == Some(K::LParen) { self.skip_balanced(p + 1) } else { p + 1 };
                }
                Some(K::Eq) => {
                    match (self.k(p + 1), self.kw(p + 1)) {
                        (Some(K::Number), _) if self.txt(p + 1) == "0" => {
                            flags.insert(SymbolFlags::PURE | SymbolFlags::VIRTUAL);
                        }
                        (_, Kw::Default) => flags.insert(SymbolFlags::DEFAULTED),
                        (_, Kw::Delete) => flags.insert(SymbolFlags::DELETED),
                        _ => {}
                    }
                    p += 2;
                }
                Some(K::Colon) => {
                    // Constructor initializer list: runs to the body's `{`.
                    p += 1;
                    loop {
                        match self.k(p) {
                            None | Some(K::Semi | K::RBrace) => break,
                            Some(K::LParen | K::LBracket) => p = self.skip_balanced(p),
                            Some(K::LBrace) => {
                                if matches!(self.k(p - 1), Some(K::RParen | K::RBrace)) {
                                    break;
                                }
                                p = self.skip_balanced(p);
                            }
                            _ => p += 1,
                        }
                    }
                }
                _ => break,
            }
        }

        let (end, body) = match self.k(p) {
            Some(K::LBrace) => {
                let e = self.skip_balanced(p);
                flags.insert(SymbolFlags::DEFINITION);
                (e, Some((p, e)))
            }
            Some(K::Semi) => (p + 1, None),
            _ => {
                let e = self.recover(p);
                if e <= start {
                    self.i = start + 1;
                    return;
                }
                (e, None)
            }
        };

        let tilde = nm > start && self.t[nm - 1].kind == K::Tilde;
        let is_op = self.t[nm].kind == K::Keyword;
        let mut text = std::mem::take(&mut self.scratch);
        text.clear();
        if tilde {
            text.push('~');
        }
        if is_op {
            text.push_str("operator");
            for k in nm + 1..op_end.min(n) {
                let t = self.txt(k);
                if text.as_bytes().last().is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_') && t.as_bytes().first().is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_') {
                    text.push(' ');
                }
                text.push_str(t);
            }
        } else {
            text.push_str(self.txt(nm));
        }
        let name = self.db.intern(&text);
        self.scratch = text;

        let scope = self.scopes.last().expect("scope stack is never empty");
        let (in_record, scope_name) = (scope.record, scope.name);
        let qualified = chain < nm;
        let base_name = if tilde { self.db.intern(self.txt(nm)) } else { name };
        let kind = if tilde {
            SymbolKind::Destructor
        } else if in_record {
            if Some(base_name) == scope_name { SymbolKind::Constructor } else { SymbolKind::Method }
        } else if qualified {
            // `Foo::Foo()` is a constructor; otherwise assume a class member.
            let last_qual = (chain..nm).rev().find(|&k| self.t[k].kind == K::Ident);
            let ctor = last_qual.is_some_and(|q| self.txt(q) == self.txt(nm));
            if ctor { SymbolKind::Constructor } else { SymbolKind::Method }
        } else {
            SymbolKind::Function
        };
        let name_tok = if tilde { nm - 1 } else { nm };
        let id = self.add(kind, name, name_tok, start, end, flags);
        let open = if is_op { op_end } else { nm + 1 };
        let (arity, hash) = self.param_signature(open, close, flags.contains(SymbolFlags::CONST));
        self.db.add_signature(Signature { symbol: id, arity, hash });
        self.note_qualifier(id, chain, if tilde { nm - 1 } else { nm });
        if let Some((a, b)) = body {
            self.opaque(OpaqueKind::Body, a, b, Some(id));
        }
        self.i = end;
    }
}
