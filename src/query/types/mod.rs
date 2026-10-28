//! Lightweight, on-demand type resolution for member access (`expr.member`).
//!
//! The index stores no type text, so types are recovered by re-lexing the
//! declaration that introduced a name: a parameter or local earlier in the
//! open buffer, or a field/function of a class. The receiver expression is
//! parsed backwards from the cursor and evaluated forwards:
//!
//! * names (`x`), `this`, `new T`, `T(...)`, `make_unique<T>(...)`;
//! * calls to functions/methods (their written return type);
//! * member chains (`a.b().c`, `p->q`), `[]` on `Foo<T>` yields `T`;
//! * template arguments flow into members: for `vector<Foo> v`, `v.front()`
//!   is `Foo`; for `unordered_map<K, V>`, `m[k]` is `V` and iterating yields
//!   `pair<K, V>`. Parameters are read from the `template<...>` header, and
//!   member aliases (`value_type`, `iterator`...) are expanded;
//! * `auto x = <expr>;` by evaluating the initializer (depth-limited);
//! * smart pointers: `->` on `unique_ptr<T>` and friends yields `T`.
//!
//! Aliases are chased through their written target. Everything is best
//! effort and returns "unknown" (an empty list) rather than guessing; callers
//! then fall back to name-based matching.
//!
//! This is the one place to extend type inference.

use super::templates::template_header;
use super::{Query, sig_tokens};
use crate::lexer::{Keyword, Token, TokenKind as K};
use crate::symbols::{SymbolId, SymbolKind};

pub(super) mod signature;
mod nav;

const MAX_DEPTH: u8 = 4;
const SMART_PTRS: &[&str] = &["unique_ptr", "shared_ptr", "weak_ptr", "intrusive_ptr", "scoped_ptr", "linked_ptr", "optional"];
const MAKE_SMART: &[&str] = &["make_unique", "make_shared", "make_intrusive", "allocate_shared", "make_optional"];
/// Calls whose result has the type of their first argument.
const PASS_THROUGH: &[&str] = &["move", "forward", "as_const", "addressof", "ref", "cref"];
/// Built-in scalar type names, for telling `int` apart from a class when ranking overloads.
const PRIMS: &[&str] = &[
    "int", "unsigned", "long", "short", "char", "bool", "float", "double", "size_t", "ssize_t", "int8_t", "int16_t", "int32_t", "int64_t", "uint8_t", "uint16_t",
    "uint32_t", "uint64_t", "wchar_t", "char8_t", "char16_t", "char32_t", "intptr_t", "uintptr_t", "ptrdiff_t",
];

/// Coarse class of a built-in type: arguments convert freely within one.
fn prim_family(name: &str) -> u8 {
    match name {
        "bool" => 0,
        "char" | "wchar_t" | "char8_t" | "char16_t" | "char32_t" => 1,
        "float" | "double" => 2,
        _ => 3,
    }
}

/// Parameter types with a has-default flag, and whether the function is variadic.
type Params = (Vec<(Option<Ty>, bool)>, bool);

/// A written type reduced to what member lookup needs: its leading name and,
/// for templates, its arguments, each as normalized type text (`Foo`,
/// `vector<Foo>`; `const` and `&` dropped, `*` counted in `ptr`) so they can be parsed again
/// with [`parse_ty`] when substituted into a member's type.
///
/// `qual` is the scope the name lives in, so `a::Foo` and `b::Foo` stay apart:
/// either as written (`a`, matched against the end of a qualified name) or,
/// with a leading `::`, a full path (`::a::b`, `::` for the global scope) that
/// [`Query::qualify`] filled in for a name written unqualified.
#[derive(Clone, Default)]
struct Ty {
    head: String,
    args: Vec<String>,
    qual: String,
    /// Levels of `*` (or of `&` taken on a value): `Foo**` is 2. References are not tracked.
    ptr: u8,
}

impl Ty {
    fn text(&self) -> String {
        let name = match self.qual.as_str() {
            "" => self.head.clone(),
            "::" => format!("::{}", self.head),
            q => format!("{q}::{}", self.head),
        };
        if self.args.is_empty() { name } else { format!("{name}<{}>", self.args.join(", ")) }
    }

    /// The type reached through `->` or `*` on a smart pointer.
    fn smart_inner(&self) -> Option<Self> {
        if SMART_PTRS.contains(&self.head.as_str()) { self.args.first().and_then(|a| parse_ty(a)) } else { None }
    }
}

/// Type names an STL-style container class exposes that are rarely written
/// out in the headers we can see: what they stand for given its arguments.
fn container_alias(name: &str, recv: &Ty) -> Option<Ty> {
    let arg = |i: usize| recv.args.get(i).and_then(|a| parse_ty(a));
    let is_map = recv.head.contains("map");
    match name {
        "key_type" => arg(0),
        "mapped_type" => arg(1),
        "value_type" | "reference" | "const_reference" | "pointer" | "const_pointer" | "iterator" | "const_iterator" | "reverse_iterator"
        | "const_reverse_iterator" | "local_iterator" | "const_local_iterator"
            if is_map && recv.args.len() >= 2 =>
        {
            Some(Ty { head: "pair".to_owned(), args: recv.args[..2].to_vec(), ..Ty::default() })
        }
        "value_type" | "reference" | "const_reference" | "pointer" | "const_pointer" | "iterator" | "const_iterator" | "reverse_iterator"
        | "const_reverse_iterator" | "local_iterator" | "const_local_iterator" => arg(0),
        _ => None,
    }
}

struct Cx<'a> {
    path: &'a str,
    text: &'a str,
    scopes: &'a [SymbolId],
}

impl Cx<'_> {
    fn text_of(&self, t: &Token) -> &str {
        t.text(self.text)
    }
}

/// What the nearest earlier declaration of a name says.
enum Decl {
    Ty(Ty),
    /// `auto name = <init>`: the declaration's token index, where its
    /// initializer starts and where it ends.
    Auto { decl: usize, start: usize, end: usize },
    /// `auto name : <range>` in a range-for.
    Range { decl: usize, start: usize, end: usize },
    /// The `n`th name of a structured binding `auto [a, b] = <init>` / `: <range>`.
    Bind(usize, Box<Decl>),
}

impl Query<'_> {
    /// Symbols the type `ty` may name: those called `ty.head` that sit in the
    /// scope it was written with. An unmatched qualifier (namespace alias,
    /// inline namespace) is ignored rather than trusted.
    fn lookup_ty(&self, ty: &Ty) -> Vec<SymbolId> {
        const MAX_SCAN: usize = 4096;
        let db = &self.idx.db;
        let all = db.lookup(&ty.head);
        if ty.qual.is_empty() || all.len() <= 1 || all.len() > MAX_SCAN {
            return all.to_vec();
        }
        let (rooted, path) = ty.qual.strip_prefix("::").map_or((false, ty.qual.as_str()), |p| (true, p));
        let want = if path.is_empty() { ty.head.clone() } else { format!("{path}::{}", ty.head) };
        let tail = format!("::{want}");
        let hits: Vec<SymbolId> = all
            .iter()
            .copied()
            .filter(|&c| {
                let q = db.qualified_name(c);
                q == want || (!rooted && q.ends_with(&tail))
            })
            .collect();
        if hits.is_empty() { all.to_vec() } else { hits }
    }

    /// Pins an unqualified type name that several scopes declare to the one
    /// visible from `scope` (innermost enclosing scope first, as C++ looks
    /// names up), including inside its template arguments. Names declared in
    /// one place only are left alone: nothing to tell apart.
    fn qualify(&self, mut ty: Ty, scope: &dyn Fn() -> String) -> Ty {
        const MAX_CANDS: usize = 256;
        let db = &self.idx.db;
        let parent_of = |q: &str| q.rfind("::").map_or(String::new(), |i| q[..i].to_owned());
        let all = db.lookup(&ty.head);
        if ty.qual.is_empty() && all.len() > 1 && all.len() <= MAX_CANDS {
            let homes: Vec<String> = all
                .iter()
                .filter(|&&c| db.symbol(c).kind.is_record() || db.symbol(c).kind == SymbolKind::TypeAlias)
                .map(|&c| parent_of(&db.qualified_name(c)))
                .collect();
            if homes.iter().any(|h| *h != homes[0]) {
                let mut s = scope();
                loop {
                    if homes.contains(&s) {
                        ty.qual = format!("::{s}");
                        break;
                    }
                    if s.is_empty() {
                        break;
                    }
                    s = parent_of(&s);
                }
            }
        }
        for a in &mut ty.args {
            let stars = a.len() - a.trim_end_matches('*').len();
            if let Some(t) = parse_ty(&a[..a.len() - stars]) {
                *a = self.qualify(t, scope).text() + &"*".repeat(stars);
            }
        }
        ty
    }

    /// [`Self::qualify`] for a type written in the declaration of `id`.
    fn qualify_in(&self, ty: Ty, id: SymbolId) -> Ty {
        self.qualify(ty, &|| {
            let q = self.idx.db.qualified_name(id);
            q.rfind("::").map_or(String::new(), |i| q[..i].to_owned())
        })
    }

    /// [`Self::qualify`] for a type written at the use site described by `cx`.
    fn qualify_at(&self, cx: &Cx, ty: Ty) -> Ty {
        self.qualify(ty, &|| cx.scopes.iter().map(|&s| self.idx.db.qualified_name(s)).max_by_key(String::len).unwrap_or_default())
    }

    /// Records `ids` may denote, following type aliases (`typedef struct _X X`,
    /// `using X = Y`) through their written target. Depth-limited, and each alias
    /// is expanded at most once with a small global budget: alias targets are
    /// looked up by bare name, so a name like `type` or `T` fans out to thousands
    /// of aliases and would otherwise explode (and read every file they live in).
    pub(super) fn resolve_records(&self, ids: impl IntoIterator<Item = SymbolId>) -> Vec<SymbolId> {
        const MAX_ALIAS_EXPANSIONS: usize = 24;
        const MAX_FANOUT: usize = 32;
        let db = &self.idx.db;
        let mut out = Vec::new();
        let mut seen = rustc_hash::FxHashSet::default();
        let mut expansions = 0;
        let mut work: Vec<(SymbolId, u8)> = ids.into_iter().map(|i| (self.idx.hier.canonical(i), 0)).collect();
        while let Some((id, depth)) = work.pop() {
            if !seen.insert(id) {
                continue;
            }
            match db.symbol(id).kind {
                k if k.is_record() => out.push(id),
                SymbolKind::TypeAlias if depth < 6 && expansions < MAX_ALIAS_EXPANSIONS => {
                    expansions += 1;
                    if let Some(t) = self.alias_target(id) {
                        let cands = self.scoped_candidates(id, &t);
                        // Records first: they end the chain, so aliases are only followed when none exist.
                        let records: Vec<_> = cands.iter().copied().filter(|&c| db.symbol(c).kind.is_record()).collect();
                        if !records.is_empty() {
                            work.extend(records.into_iter().take(MAX_FANOUT).map(|c| (c, depth + 1)));
                        } else if cands.len() <= MAX_FANOUT {
                            work.extend(cands.iter().filter(|&&c| c != id).map(|&c| (c, depth + 1)));
                        }
                    }
                }
                _ => {}
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// What `name`, written inside the declaration of `from`, can refer to: the
    /// symbols named `name` in the innermost enclosing scope that has any
    /// (`n::Foo::name`, then `n::name`, then `name`). Falls back to every symbol
    /// of that name when no enclosing scope declares it (using-directives, ADL).
    fn scoped_candidates(&self, from: SymbolId, name: &str) -> Vec<SymbolId> {
        const MAX_SCAN: usize = 4096;
        let db = &self.idx.db;
        let all = db.lookup(name);
        if all.len() <= 1 || all.len() > MAX_SCAN {
            return all.to_vec();
        }
        let qualified = db.qualified_name(from);
        let mut scope = qualified.rfind("::").map_or("", |i| &qualified[..i]);
        let parents: Vec<(SymbolId, String)> = all.iter().filter(|&&c| c != from).map(|&c| (c, db.qualified_name(c))).collect();
        loop {
            let hits: Vec<SymbolId> = parents
                .iter()
                .filter(|(_, q)| q.rfind("::").map_or("", |i| &q[..i]) == scope)
                .map(|&(c, _)| c)
                .collect();
            if !hits.is_empty() {
                return hits;
            }
            if scope.is_empty() {
                return all.to_vec();
            }
            scope = scope.rfind("::").map_or("", |i| &scope[..i]);
        }
    }

    /// The type name an alias is written in terms of, read from its declaration.
    fn alias_target(&self, id: SymbolId) -> Option<String> {
        let db = &self.idx.db;
        let s = db.symbol(id);
        let text = self.texts.text(db.files.path(s.file))?;
        let src = text.as_str().get(s.extent_start as usize..s.extent_end as usize)?;
        let toks = sig_tokens(src);
        let mut i = 0;
        if toks.first().is_some_and(|t| t.is_keyword(Keyword::Using)) {
            i = toks.iter().position(|t| t.kind == K::Eq)? + 1;
        }
        while i < toks.len() {
            match toks[i].kind {
                K::Keyword => {
                    let elaborated = matches!(toks[i].keyword, Keyword::Struct | Keyword::Class | Keyword::Union | Keyword::Enum);
                    if elaborated && toks.get(i + 1).is_some_and(|t| t.kind == K::LBrace) {
                        return None;
                    }
                    i += 1;
                }
                K::Ident => {
                    let mut j = i;
                    while toks.get(j + 1).is_some_and(|t| t.kind == K::ColonColon) && toks.get(j + 2).is_some_and(|t| t.kind == K::Ident) {
                        j += 2;
                    }
                    let name = toks[j].text(src);
                    return (name != db.name(id)).then(|| name.to_owned());
                }
                _ => return None,
            }
        }
        None
    }

    /// Classes the expression ending at `end` (the position of the `.` or
    /// `->` that follows it) may denote.
    pub(super) fn receiver_types(&self, path: &str, text: &str, end: u32, scopes: &[SymbolId]) -> Vec<SymbolId> {
        let end = (end as usize).min(text.len());
        let db = &self.idx.db;
        let toks = sig_tokens(&text[..end]);
        let start = expr_start(&toks);
        let expr = &toks[start..];
        if expr.is_empty() {
            return Vec::new();
        }
        if expr.len() == 1 && expr[0].text(text) == "this" {
            return scopes.iter().copied().filter(|&c| db.symbol(c).kind.is_record()).take(1).collect();
        }
        let cx = Cx { path, text, scopes };
        let Some(mut ty) = self.expr_ty(&cx, &toks[..start], expr, 0) else { return Vec::new() };
        if text[end..].starts_with("->") {
            ty = self.arrow_ty(&cx, ty);
        }
        self.resolve_records(self.lookup_ty(&ty))
    }

    /// Type of the expression `expr`; `prefix` are the tokens before it, in
    /// which variables are looked up.
    fn expr_ty(&self, cx: &Cx, prefix: &[Token], expr: &[Token], depth: u8) -> Option<Ty> {
        if depth > MAX_DEPTH {
            return None;
        }
        if let Some(q) = top_level(expr, K::Question) {
            let (yes, no) = match top_level(&expr[q + 1..], K::Colon) {
                Some(c) => (&expr[q + 1..q + 1 + c], &expr[q + 2 + c..]),
                None => (&expr[q + 1..], &expr[..0]),
            };
            return self.expr_ty(cx, prefix, yes, depth + 1).or_else(|| self.expr_ty(cx, prefix, no, depth + 1));
        }
        let mut start = 0;
        let mut derefs = 0;
        let mut addrs = 0u8;
        while let Some(t) = expr.get(start).filter(|t| matches!(t.kind, K::Star | K::Amp | K::AmpAmp) || t.is_keyword(Keyword::Const)) {
            derefs += (t.kind == K::Star) as usize;
            addrs += (t.kind == K::Amp) as u8;
            start += 1;
        }
        let (mut ty, mut i) = self.primary_ty(cx, prefix, expr, start, depth)?;
        while let Some(t) = expr.get(i) {
            match t.kind {
                K::Dot | K::Arrow => {
                    let recv = if t.kind == K::Arrow { self.arrow_ty(cx, ty) } else { ty };
                    let n = expr.get(i + 1).filter(|t| t.kind == K::Ident)?;
                    i += 2;
                    if expr.get(i).is_some_and(|t| t.kind == K::Lt) {
                        i = matching_fwd(expr, i)? + 1;
                    }
                    let mut arity = None;
                    if expr.get(i).is_some_and(|t| t.kind == K::LParen) {
                        let close = matching_fwd(expr, i)?;
                        let inner = &expr[i + 1..close];
                        arity = Some(if inner.is_empty() { 0 } else { 1 + (0..inner.len()).filter(|&k| inner[k].kind == K::Comma && matching_back_depth0(inner, k)).count() });
                        i = close + 1;
                    }
                    ty = self.operator_ty_n(cx, &recv, cx.text_of(n), arity)?;
                }
                K::LBracket => {
                    i = matching_fwd(expr, i)? + 1;
                    ty = if ty.ptr > 0 {
                        Ty { ptr: ty.ptr - 1, ..ty }
                    } else {
                        match self.operator_ty(cx, &ty, "operator[]").filter(|t| self.resolves(t)) {
                            Some(t) => t,
                            None => ty.args.first().and_then(|a| parse_ty(a)).unwrap_or(ty),
                        }
                    };
                }
                _ => break,
            }
        }
        for _ in 0..derefs {
            ty = self.deref_ty(cx, ty);
        }
        ty.ptr = ty.ptr.saturating_add(addrs);
        let Some(t) = expr.get(i) else { return Some(ty) };
        match t.kind {
            K::Eq | K::Comma | K::Semi => Some(ty),
            // Binary operator: only overloaded ones on class types yield members; arithmetic keeps the left type.
            _ => self.operator_ty(cx, &ty, &format!("operator{}", cx.text_of(t))).or_else(|| PRIMS.contains(&ty.head.as_str()).then_some(ty)),
        }
    }

    /// A primary expression at `expr[i..]` and the index just after it.
    fn primary_ty(&self, cx: &Cx, prefix: &[Token], expr: &[Token], mut i: usize, depth: u8) -> Option<(Ty, usize)> {
        let tx = |t: &Token| t.text(cx.text);
        if let Some(t) = expr.get(i).filter(|t| matches!(t.kind, K::String | K::Char | K::Number)) {
            let (head, ptr) = match t.kind {
                K::Number if tx(t).starts_with("0x") || !tx(t).contains(['.', 'e']) => ("int", 0),
                K::Number => ("double", 0),
                K::String => ("char", 1),
                _ => ("char", 0),
            };
            return Some((Ty { head: head.to_owned(), ptr, ..Ty::default() }, i + 1));
        }
        if expr.get(i).is_some_and(|t| t.kind == K::LParen) {
            let j = matching_fwd(expr, i)?;
            let inner = &expr[i + 1..j];
            let casting = expr.get(j + 1).is_some_and(|t| matches!(t.kind, K::Ident | K::LParen));
            if casting && let Some(t) = declared_type_tail(inner, cx.text).filter(|t| self.resolves(t)) {
                return Some((t, expr.len()));
            }
            return Some((self.expr_ty(cx, prefix, inner, depth + 1)?, j + 1));
        }
        let is_new = expr.get(i).is_some_and(|t| t.kind == K::Ident && tx(t) == "new");
        i += is_new as usize;

        let mut path: Vec<&str> = Vec::new();
        let mut targs: Vec<String> = Vec::new();
        loop {
            let t = expr.get(i).filter(|t| t.kind == K::Ident)?;
            path.push(tx(t));
            i += 1;
            if expr.get(i).is_some_and(|t| t.kind == K::Lt) {
                let close = matching_fwd(expr, i)?;
                if targs.is_empty() {
                    targs = split_args(&expr[i + 1..close], cx.text);
                }
                i = close + 1;
            }
            if expr.get(i).is_some_and(|t| t.kind == K::ColonColon) {
                i += 1;
            } else {
                break;
            }
        }
        let name = path.pop()?;
        let open = i;
        let called = expr.get(i).is_some_and(|t| matches!(t.kind, K::LParen | K::LBrace));
        if called {
            i = matching_fwd(expr, i)? + 1;
        }
        let ty = if is_new {
            self.qualify_at(cx, Ty { head: name.to_owned(), args: targs, qual: path.last().map(|q| q.to_string()).unwrap_or_default(), ptr: 1 })
        } else if called && PASS_THROUGH.contains(&name) {
            let args = &expr[open + 1..i - 1];
            let first = &args[..top_level(args, K::Comma).unwrap_or(args.len())];
            self.expr_ty(cx, prefix, first, depth + 1)?
        } else if called {
            let args = (expr[open].kind == K::LParen).then(|| &expr[open + 1..i - 1]);
            self.call_ty(cx, prefix, depth, path.last().copied(), name, &targs, args)?
        } else if let Some(q) = path.last() {
            let owners = self.resolve_records(self.idx.db.lookup(q).iter().copied());
            self.member_ty(cx, &owners, name, None)?
        } else {
            self.ident_ty(cx, prefix, name, depth)?
        };
        Some((ty, i))
    }

    fn resolves(&self, ty: &Ty) -> bool {
        !self.resolve_records(self.lookup_ty(ty)).is_empty()
    }

    /// Member `name` (usually an `operator…`) of the class `ty` names, or its bases.
    fn operator_ty(&self, cx: &Cx, ty: &Ty, name: &str) -> Option<Ty> {
        self.operator_ty_n(cx, ty, name, None)
    }

    fn operator_ty_n(&self, cx: &Cx, ty: &Ty, name: &str, arity: Option<usize>) -> Option<Ty> {
        let ty = &self.expand_alias(cx, ty.clone());
        let mut owners = self.resolve_records(self.lookup_ty(ty));
        self.choose_specialization(&mut owners, (!ty.args.is_empty()).then_some(&ty.args[..]));
        self.member_ty_n(cx, &owners, name, Some(ty), arity)
    }

    /// Replaces an alias used as a type (`using Vec = eastl::vector<Foo>;`,
    /// `typedef ... Vec2;`) by what it spells, so its template arguments survive.
    fn expand_alias(&self, cx: &Cx, mut ty: Ty) -> Ty {
        let db = &self.idx.db;
        for _ in 0..4 {
            if !ty.args.is_empty() {
                break;
            }
            let cands = self.lookup_ty(&ty);
            if cands.iter().any(|&c| db.symbol(c).kind.is_record()) {
                break;
            }
            let aliases = cands.iter().copied().filter(|&c| db.symbol(c).kind == SymbolKind::TypeAlias);
            let preferred = aliases.clone().find(|c| db.symbol(*c).parent.is_some_and(|p| cx.scopes.contains(&p)));
            let next = preferred.into_iter().chain(aliases.take(8)).find_map(|a| self.alias_full_ty(cx, a).filter(|t| t.head != ty.head));
            match next {
                Some(t) => ty = Ty { ptr: t.ptr + ty.ptr, ..t },
                None => break,
            }
        }
        ty
    }

    /// The type reached through `->`.
    fn arrow_ty(&self, cx: &Cx, ty: Ty) -> Ty {
        let ty = self.expand_alias(cx, ty);
        if ty.ptr > 0 {
            return Ty { ptr: ty.ptr - 1, ..ty };
        }
        if let Some(t) = ty.smart_inner() {
            return t;
        }
        // `operator->` returns a pointer to the object.
        match self.operator_ty(cx, &ty, "operator->").filter(|t| self.resolves(t)) {
            Some(t) => Ty { ptr: t.ptr.saturating_sub(1), ..t },
            None => ty,
        }
    }

    /// The type of `*expr`.
    fn deref_ty(&self, cx: &Cx, ty: Ty) -> Ty {
        let ty = self.expand_alias(cx, ty);
        if ty.ptr > 0 {
            return Ty { ptr: ty.ptr - 1, ..ty };
        }
        if let Some(t) = ty.smart_inner() {
            return t;
        }
        self.operator_ty(cx, &ty, "operator*").filter(|t| self.resolves(t)).unwrap_or(ty)
    }

    /// Element type when iterating over a value of type `ty` in a range-for.
    fn element_ty(&self, cx: &Cx, ty: Ty) -> Option<Ty> {
        let ty = self.expand_alias(cx, ty);
        if ty.head.contains("map") && ty.args.len() >= 2 {
            return container_alias("value_type", &ty);
        }
        if let Some(a) = ty.args.first() {
            return parse_ty(a);
        }
        match self.operator_ty(cx, &ty, "begin") {
            Some(it) => Some(self.deref_ty(cx, it)),
            None => Some(ty), // plain C array
        }
    }

    /// Type of a name declared with `auto` (possibly as one element of a structured binding).
    fn auto_decl_ty(&self, cx: &Cx, prefix: &[Token], d: Decl, depth: u8) -> Option<Ty> {
        match d {
            Decl::Ty(t) => Some(self.qualify_at(cx, t)),
            Decl::Auto { decl, start, end } => self.expr_ty(cx, &prefix[..decl], &prefix[start..end], depth + 1),
            Decl::Range { decl, start, end } => {
                let container = self.expr_ty(cx, &prefix[..decl], &prefix[start..end], depth + 1)?;
                self.element_ty(cx, container)
            }
            Decl::Bind(n, inner) => {
                let whole = self.auto_decl_ty(cx, prefix, *inner, depth)?;
                self.binding_ty(cx, &whole, n)
            }
        }
    }

    /// Type of the `n`th name in `auto [a, b, ...] = <value of type ty>`.
    fn binding_ty(&self, cx: &Cx, ty: &Ty, n: usize) -> Option<Ty> {
        let ty = &self.expand_alias(cx, ty.clone());
        if matches!(ty.head.as_str(), "pair" | "tuple") {
            return ty.args.get(n).and_then(|a| parse_ty(a));
        }
        if ty.head == "array" {
            return ty.args.first().and_then(|a| parse_ty(a));
        }
        let db = &self.idx.db;
        let mut owners = self.resolve_records(self.lookup_ty(ty));
        self.choose_specialization(&mut owners, (!ty.args.is_empty()).then_some(&ty.args[..]));
        let owner = *owners.first()?;
        let field = db.children(owner).iter().copied().filter(|&m| db.symbol(m).kind == SymbolKind::Field && !db.symbol(m).flags.contains(crate::symbols::SymbolFlags::STATIC)).nth(n)?;
        let t = self.decl_ty(cx, field)?;
        Some(self.instantiate(cx, t, owner, ty))
    }

    /// Inferred type of the `auto` local `name` declared at `decl_offset`, as written (`Foo<Bar>`).
    pub(super) fn local_type(&self, path: &str, text: &str, name: &str, decl_offset: u32) -> Option<String> {
        // Keep tokens up to the end of the declaring statement (or range-for header).
        let mut depth = 0i32;
        let mut cut = text.len();
        for (k, &b) in text.as_bytes().iter().enumerate().skip(decl_offset as usize) {
            match b {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth < 0 {
                        cut = k + 1;
                        break;
                    }
                }
                b';' if depth <= 0 => {
                    cut = k + 1;
                    break;
                }
                _ => {}
            }
        }
        let scopes = self.scopes_at(self.idx.file_index(path), decl_offset);
        let cx = Cx { path, text, scopes: &scopes };
        let toks = sig_tokens(&text[..cut]);
        let ty = self.ident_ty(&cx, &toks, name, 0)?;
        Some(ty.text())
    }

    /// Type of the variable, parameter, field or `this` called `name`.
    fn ident_ty(&self, cx: &Cx, prefix: &[Token], name: &str, depth: u8) -> Option<Ty> {
        let db = &self.idx.db;
        if name == "this" {
            let c = cx.scopes.iter().copied().find(|&c| db.symbol(c).kind.is_record())?;
            return Some(self.ty_of(c));
        }
        match declared_before(prefix, cx.text, name) {
            Some(Decl::Ty(t)) => return Some(self.qualify_at(cx, t)),
            Some(d) => return self.auto_decl_ty(cx, prefix, d, depth),
            None => {}
        }
        let (in_scope, in_bases) = self.scope_set(cx.scopes);
        for owner in in_scope.into_iter().chain(in_bases) {
            for &m in db.children(owner) {
                if db.name(m) == name && matches!(db.symbol(m).kind, SymbolKind::Field | SymbolKind::Variable) {
                    return self.decl_ty(cx, m);
                }
            }
        }
        None
    }

    /// Result type of calling `name` (optionally qualified by `qual`). With the
    /// call's `args` (and the expression `prefix` they are evaluated in), the
    /// overload whose parameter types fit the argument types best is chosen.
    #[allow(clippy::too_many_arguments)]
    fn call_ty(&self, cx: &Cx, prefix: &[Token], depth: u8, qual: Option<&str>, name: &str, targs: &[String], args: Option<&[Token]>) -> Option<Ty> {
        if name.ends_with("_cast") {
            return targs.first().and_then(|a| parse_ty(a));
        }
        if MAKE_SMART.contains(&name) {
            return targs.first().map(|a| Ty { head: "unique_ptr".to_owned(), args: vec![a.clone()], ..Ty::default() });
        }
        let db = &self.idx.db;
        if !self.resolve_records(db.lookup(name).iter().copied()).is_empty() {
            return Some(self.qualify_at(cx, Ty { head: name.to_owned(), args: targs.to_vec(), qual: qual.unwrap_or_default().to_owned(), ptr: 0 }));
        }
        let (in_scope, in_bases) = self.scope_set(cx.scopes);
        let is_fn = |c: SymbolId| db.name(c) == name && matches!(db.symbol(c).kind, SymbolKind::Function | SymbolKind::Method);
        let qual_ok = |c: SymbolId| qual.is_none_or(|q| db.symbol(c).parent.is_some_and(|p| db.name(p) == q));
        let mut cands: Vec<SymbolId> = in_scope.into_iter().chain(in_bases).flat_map(|o| db.children(o).iter().copied()).filter(|&c| is_fn(c) && qual_ok(c)).collect();
        // Methods of unrelated classes are not candidates for an unqualified call.
        let global = db.lookup(name).iter().copied().filter(|&c| is_fn(c) && qual_ok(c));
        let global: Vec<SymbolId> = global.collect();
        let free = |c: &SymbolId| qual.is_some() || db.symbol(*c).parent.is_none_or(|p| !db.symbol(p).kind.is_record());
        cands.extend(global.iter().copied().filter(free));
        if cands.is_empty() {
            cands = global;
        }
        let Some(args) = args.filter(|_| cands.len() > 1) else {
            return cands.into_iter().find_map(|c| self.decl_ty(cx, c));
        };
        let arg_tys: Vec<Option<Ty>> = split_call_args(args).into_iter().map(|a| self.expr_ty(cx, prefix, a, depth + 1)).collect();
        let mut best: Option<((bool, i32, bool), Ty)> = None;
        for c in cands {
            let Some(ret) = self.decl_ty(cx, c) else { continue };
            let (fits, score) = self.call_fit(cx, c, &arg_tys);
            let key = (fits, score, ret.head != "void");
            if best.as_ref().is_none_or(|(k, _)| key > *k) {
                best = Some((key, ret));
            }
        }
        best.map(|(_, t)| t)
    }

    /// Of equally ranked overloads of the call at `toks[i]`, those whose
    /// parameter types fit the arguments best. Unchanged when no argument type is known.
    pub(super) fn best_overloads(&self, path: &str, text: &str, toks: &[Token], i: usize, cands: Vec<SymbolId>) -> Vec<SymbolId> {
        let open = i + 1;
        let Some(close) = matching_fwd(toks, open) else { return cands };
        let scopes = self.scopes_at(self.idx.file_index(path), toks[i].start);
        let cx = Cx { path, text, scopes: &scopes };
        let arg_tys: Vec<Option<Ty>> = split_call_args(&toks[open + 1..close]).into_iter().map(|a| self.expr_ty(&cx, &toks[..=open], a, 1)).collect();
        if arg_tys.iter().all(Option::is_none) {
            return cands;
        }
        let fits: Vec<(SymbolId, (bool, i32))> = cands.iter().map(|&c| (c, self.call_fit(&cx, c, &arg_tys))).collect();
        let best = fits.iter().map(|f| f.1).max();
        fits.into_iter().filter(|f| Some(f.1) == best).map(|f| f.0).collect()
    }

    /// Parameter types of the function `s` declares (and whether each has a
    /// default), and whether it is variadic.
    fn param_tys(&self, cx: &Cx, s: &crate::symbols::Symbol) -> Option<Params> {
        self.source_of(cx, s, |src| {
            let head = src.get(s.name_offset as usize..(s.name_offset as usize + 600).min(s.extent_end as usize).min(src.len()))?;
            let toks = sig_tokens(head);
            let open = toks.iter().position(|t| t.kind == K::LParen)?;
            let close = matching_fwd(&toks, open)?;
            let inner = &toks[open + 1..close];
            let mut out: Vec<(Option<Ty>, bool)> = Vec::new();
            if !(inner.is_empty() || (inner.len() == 1 && inner[0].text(head) == "void")) {
                for p in split_call_args(inner) {
                    let def = top_level(p, K::Eq);
                    let mut ty = &p[..def.unwrap_or(p.len())];
                    let named = |t: &Token| matches!(t.kind, K::Ident | K::Gt | K::Star | K::Amp | K::AmpAmp);
                    if ty.len() >= 2 && ty[ty.len() - 1].kind == K::Ident && named(&ty[ty.len() - 2]) {
                        ty = &ty[..ty.len() - 1];
                    }
                    out.push((declared_type_tail(ty, head), def.is_some()));
                }
            }
            Some((out, inner.iter().any(|t| t.kind == K::Ellipsis)))
        })
    }

    /// Whether `c` accepts `args.len()` arguments and how well its parameter
    /// types match the argument types (higher is better).
    fn call_fit(&self, cx: &Cx, c: SymbolId, args: &[Option<Ty>]) -> (bool, i32) {
        let Some((params, variadic)) = self.param_tys(cx, self.idx.db.symbol(c)) else {
            return (true, 0);
        };
        let n = args.len();
        let required = params.iter().filter(|p| !p.1).count();
        let fits = n >= required && (variadic || n <= params.len());
        let mut score = 0;
        for (a, (p, _)) in args.iter().zip(&params) {
            let (Some(a), Some(p)) = (a, p) else { continue };
            score += self.arg_fit(cx, a, p, true);
        }
        (fits, score)
    }

    /// How well an argument of type `arg` fits a parameter of type `param`:
    /// 2 same type, 1 derived-to-base or compatible built-ins, 0 through an
    /// implicit conversion (when `conv`) or unknown, negative for a mismatch.
    fn arg_fit(&self, cx: &Cx, arg: &Ty, param: &Ty, conv: bool) -> i32 {
        let rec = |t: &Ty| self.resolve_records(self.lookup_ty(t)).first().copied();
        let (ra, rp) = (rec(arg), rec(param));
        if arg.ptr != param.ptr {
            if param.head == "void" && param.ptr > 0 && arg.ptr > 0 {
                return 0;
            }
            if conv && param.ptr == 0 && rp.is_some_and(|p| self.converts_via_ctor(cx, p, arg)) {
                return 0;
            }
            // A bare template parameter or unresolved name could be a pointer itself.
            let known = |t: &Ty, r: Option<SymbolId>| r.is_some() || t.head == "void" || PRIMS.contains(&t.head.as_str());
            return if known(arg, ra) && known(param, rp) { -4 } else { 0 };
        }
        if arg.head == param.head && (ra.is_none() || rp.is_none() || ra == rp) {
            return 2;
        }
        let (ap, pp) = (PRIMS.contains(&arg.head.as_str()), PRIMS.contains(&param.head.as_str()));
        match (ra, rp) {
            (Some(a), Some(p)) => {
                if self.idx.hier.is_a(a, p) {
                    1
                } else if conv && arg.ptr == 0 && (self.converts_via_ctor(cx, p, arg) || self.converts_via_operator(cx, a, param)) {
                    0
                } else {
                    -3
                }
            }
            (Some(a), None) if pp => {
                if conv && arg.ptr == 0 && self.converts_via_operator(cx, a, param) { 0 } else { -3 }
            }
            (None, Some(p)) if ap => {
                if conv && self.converts_via_ctor(cx, p, arg) { 0 } else { -3 }
            }
            (None, None) if ap && pp => {
                if prim_family(&arg.head) == prim_family(&param.head) { 1 } else { -1 }
            }
            _ => 0,
        }
    }

    /// Whether a value of type `arg` converts to the class `target` through a
    /// non-explicit constructor callable with one argument.
    fn converts_via_ctor(&self, cx: &Cx, target: SymbolId, arg: &Ty) -> bool {
        let db = &self.idx.db;
        db.children(target).iter().copied().filter(|&m| db.symbol(m).kind == SymbolKind::Constructor && !db.symbol(m).flags.contains(crate::symbols::SymbolFlags::EXPLICIT)).any(|m| {
            let Some((params, _)) = self.param_tys(cx, db.symbol(m)) else { return false };
            let required = params.iter().filter(|p| !p.1).count();
            required <= 1 && params.first().and_then(|p| p.0.as_ref()).is_some_and(|p| self.arg_fit(cx, arg, p, false) > 0)
        })
    }

    /// Whether the class `source` has a non-explicit conversion operator to `target`.
    fn converts_via_operator(&self, cx: &Cx, source: SymbolId, target: &Ty) -> bool {
        let db = &self.idx.db;
        let mut owners = vec![source];
        self.idx.hier.all_bases(source, &mut owners);
        owners.iter().any(|&o| {
            db.children(o).iter().any(|&m| {
                let s = db.symbol(m);
                if s.kind != SymbolKind::Method || s.flags.contains(crate::symbols::SymbolFlags::EXPLICIT) {
                    return false;
                }
                // Conversion operators are named by their target type: `operator int`, `operator const char*`.
                let Some(to) = db.name(m).strip_prefix("operator ") else { return false };
                !matches!(to, "new" | "delete" | "new[]" | "delete[]" | "co_await") && parse_ty(to).is_some_and(|t| self.arg_fit(cx, &t, target, false) > 0)
            })
        })
    }

    /// Type of member `name` (field type or method return type) of `owners` or their bases.
    /// `recv` is the instantiated type the member is accessed on; template
    /// parameters in the member's type are replaced by its arguments.
    fn member_ty(&self, cx: &Cx, owners: &[SymbolId], name: &str, recv: Option<&Ty>) -> Option<Ty> {
        self.member_ty_n(cx, owners, name, recv, None)
    }

    /// `(required, maximum)` argument counts of the function declared by `s`.
    fn arity_range(&self, cx: &Cx, s: &crate::symbols::Symbol) -> Option<(usize, usize)> {
        self.source_of(cx, s, |src| {
            let head = src.get(s.name_offset as usize..(s.name_offset as usize + 600).min(s.extent_end as usize).min(src.len()))?;
            let toks = sig_tokens(head);
            let open = toks.iter().position(|t| t.kind == K::LParen)?;
            let close = matching_fwd(&toks, open)?;
            let inner = &toks[open + 1..close];
            if inner.is_empty() || (inner.len() == 1 && inner[0].text(head) == "void") {
                return Some((0, 0));
            }
            let (mut depth, mut total, mut defaulted, mut has_default) = (0i32, 1usize, 0usize, false);
            for t in inner {
                match t.kind {
                    K::Lt | K::LParen | K::LBracket | K::LBrace => depth += 1,
                    K::Gt | K::RParen | K::RBracket | K::RBrace => depth -= 1,
                    K::Eq if depth == 0 => has_default = true,
                    K::Comma if depth == 0 => {
                        total += 1;
                        defaulted += has_default as usize;
                        has_default = false;
                    }
                    _ => {}
                }
            }
            defaulted += has_default as usize;
            let variadic = inner.iter().any(|t| t.kind == K::Ellipsis);
            Some((total - defaulted, if variadic { usize::MAX } else { total }))
        })
    }

    /// [`Self::member_ty`] for a call with `arity` arguments: among overloads
    /// the one that accepts that many arguments and returns a value wins.
    fn member_ty_n(&self, cx: &Cx, owners: &[SymbolId], name: &str, recv: Option<&Ty>, arity: Option<usize>) -> Option<Ty> {
        let db = &self.idx.db;
        // Each class to search with the type its template parameters are bound by
        // (bases get theirs from the arguments written in the derived class's base list).
        let mut chain: Vec<(SymbolId, Option<Ty>)> = owners.iter().map(|&o| (o, recv.cloned())).collect();
        let mut i = 0;
        while i < chain.len() && chain.len() < 64 {
            let (o, r) = chain[i].clone();
            for &b in self.idx.hier.bases(o) {
                if !chain.iter().any(|(c, _)| *c == b) {
                    let bty = r.as_ref().and_then(|r| self.base_ty(cx, o, r, b));
                    chain.push((b, bty));
                }
            }
            i += 1;
        }
        chain.iter().find_map(|(o, r)| {
            let mut best: Option<((bool, bool), Ty)> = None;
            for m in db.children(*o).iter().copied().filter(|&m| db.name(m) == name && matches!(db.symbol(m).kind, SymbolKind::Field | SymbolKind::Variable | SymbolKind::Function | SymbolKind::Method)) {
                let Some(ty) = self.decl_ty(cx, m) else { continue };
                let ty = match r {
                    Some(r) => self.instantiate(cx, ty, *o, r),
                    None => ty,
                };
                let is_fn = matches!(db.symbol(m).kind, SymbolKind::Function | SymbolKind::Method);
                let fits = match (arity, is_fn) {
                    (Some(n), true) => self.arity_range(cx, db.symbol(m)).is_none_or(|(lo, hi)| (lo..=hi).contains(&n)),
                    _ => true,
                };
                let key = (fits, ty.head != "void");
                if best.as_ref().is_none_or(|(k, _)| key > *k) {
                    best = Some((key, ty));
                }
            }
            best.map(|(_, t)| t)
        })
    }

    /// `base` as written in the base list of `derived` (`: public vector<T, A>`),
    /// with `derived`'s template parameters replaced from `recv`.
    fn base_ty(&self, cx: &Cx, derived: SymbolId, recv: &Ty, base: SymbolId) -> Option<Ty> {
        let db = &self.idx.db;
        let s = db.symbol(derived);
        let bname = db.name(base);
        let written = self.source_of(cx, s, |src| {
            let head = src.get(s.name_offset as usize..(s.name_offset as usize + 600).min(s.extent_end as usize).min(src.len()))?;
            let toks = sig_tokens(head);
            let colon = toks.iter().position(|t| t.kind == K::Colon)?;
            let end = toks.iter().position(|t| t.kind == K::LBrace).unwrap_or(toks.len());
            let i = (colon..end).find(|&i| toks[i].kind == K::Ident && toks[i].text(head) == bname && toks.get(i + 1).is_some_and(|t| t.kind == K::Lt))?;
            let close = matching_fwd(&toks, i + 1)?;
            Some(split_args(&toks[i + 2..close], head))
        })?;
        Some(self.instantiate(cx, Ty { head: bname.to_owned(), args: written, qual: self.ty_of(base).qual, ptr: 0 }, derived, recv))
    }

    /// `ty`, written inside template `owner`, as seen from `recv = owner<args>`.
    fn instantiate(&self, cx: &Cx, ty: Ty, owner: SymbolId, recv: &Ty) -> Ty {
        let params = self.template_params(cx, owner);
        if params.is_empty() {
            return ty;
        }
        if self.idx.db.symbol(owner).flags.contains(crate::symbols::SymbolFlags::SPECIALIZATION) {
            // `struct X<T*>`: each parameter is bound by where it appears in the written arguments.
            let written = self.spec_args(owner);
            let args = params
                .iter()
                .map(|p| {
                    let k = written.iter().position(|w| w.trim_end_matches('*') == p)?;
                    let stars = written[k].len() - p.len();
                    let a = recv.args.get(k)?;
                    Some(a[..a.len().saturating_sub(stars)].to_owned())
                })
                .map(Option::unwrap_or_default)
                .collect();
            return self.subst(cx, ty, owner, &params, &Ty { head: recv.head.clone(), args, qual: recv.qual.clone(), ptr: recv.ptr }, 0);
        }
        self.subst(cx, ty, owner, &params, recv, 0)
    }

    fn subst(&self, cx: &Cx, ty: Ty, owner: SymbolId, params: &[String], recv: &Ty, depth: u8) -> Ty {
        if depth > 6 {
            return ty;
        }
        if let Some(i) = params.iter().position(|p| *p == ty.head) {
            return recv.args.get(i).and_then(|a| parse_ty(a)).map(|t| Ty { ptr: t.ptr + ty.ptr, ..t }).unwrap_or(ty);
        }
        if ty.args.is_empty() {
            let own = self.idx.db.children(owner).iter().copied().find(|&m| self.idx.db.name(m) == ty.head && self.idx.db.symbol(m).kind == SymbolKind::TypeAlias);
            let expanded = match own {
                Some(m) => self.alias_full_ty(cx, m),
                None => container_alias(&ty.head, &self.subst_args(cx, recv, owner, params, recv, depth)),
            };
            if let Some(t) = expanded {
                return self.subst(cx, t, owner, params, recv, depth + 1);
            }
        }
        self.subst_args(cx, &ty, owner, params, recv, depth)
    }

    fn subst_args(&self, cx: &Cx, ty: &Ty, owner: SymbolId, params: &[String], recv: &Ty, depth: u8) -> Ty {
        let args = ty
            .args
            .iter()
            .map(|a| match parse_ty(a) {
                Some(t) => self.subst(cx, t, owner, params, recv, depth + 1).text(),
                None => a.clone(),
            })
            .collect();
        Ty { head: ty.head.clone(), args, qual: ty.qual.clone(), ptr: ty.ptr }
    }

    /// Source text of the file declaring `s`.
    fn source_of<R>(&self, cx: &Cx, s: &crate::symbols::Symbol, f: impl FnOnce(&str) -> Option<R>) -> Option<R> {
        let fpath = self.idx.db.files.path(s.file);
        let t = if fpath == cx.path { None } else { self.texts.text(fpath) };
        f(t.as_ref().map(|t| t.as_str()).unwrap_or(cx.text))
    }

    /// Names of the template parameters of `id`, read from the `template<...>`
    /// header written just before its declaration.
    fn template_params(&self, cx: &Cx, id: SymbolId) -> Vec<String> {
        let s = self.idx.db.symbol(id);
        if !s.flags.contains(crate::symbols::SymbolFlags::TEMPLATE) {
            return Vec::new();
        }
        self.source_of(cx, s, |src| template_header(src, s.extent_start as usize).map(|h| h.params.into_iter().map(|p| p.name).collect())).unwrap_or_default()
    }

    /// The type a `using X = T;` / `typedef T X;` declaration spells, with its arguments.
    fn alias_full_ty(&self, cx: &Cx, id: SymbolId) -> Option<Ty> {
        let s = self.idx.db.symbol(id);
        let name = self.idx.db.name(id);
        self.source_of(cx, s, |text| {
            let src = text.get(s.extent_start as usize..s.extent_end as usize)?;
            let toks = sig_tokens(src);
            let (lo, hi) = if toks.first()?.is_keyword(Keyword::Using) {
                let eq = toks.iter().position(|t| t.kind == K::Eq)?;
                (eq + 1, toks.len() - toks.last().is_some_and(|t| t.kind == K::Semi) as usize)
            } else {
                (1, toks.iter().rposition(|t| t.kind == K::Ident && t.text(src) == name)?)
            };
            declared_type_tail(toks.get(lo..hi)?, src)
        })
        .map(|t| self.qualify_in(t, id))
    }

    /// Written type of a declaration: a field/variable's type or a function's return type.
    fn decl_ty(&self, cx: &Cx, id: SymbolId) -> Option<Ty> {
        let s = self.idx.db.symbol(id);
        self.source_of(cx, s, |src| {
            let slice = src.get(s.extent_start.min(s.name_offset) as usize..s.name_offset as usize)?;
            let toks = sig_tokens(slice);
            declared_type_tail(&toks, slice).filter(|t| t.head != "auto")
        })
        .map(|t| self.qualify_in(t, id))
    }

    /// The class (or other type) `id` itself, written fully qualified.
    fn ty_of(&self, id: SymbolId) -> Ty {
        let db = &self.idx.db;
        let q = db.qualified_name(id);
        let scope = q.rfind("::").map_or("", |i| &q[..i]);
        Ty { head: db.name(id).to_owned(), args: Vec::new(), qual: format!("::{scope}"), ptr: 0 }
    }
}

/// Whether the token at `at` sits outside any bracket pair within `toks`.
fn matching_back_depth0(toks: &[Token], at: usize) -> bool {
    let mut depth = 0i32;
    for t in &toks[..at] {
        match t.kind {
            K::Lt | K::LParen | K::LBracket | K::LBrace => depth += 1,
            K::Gt | K::RParen | K::RBracket | K::RBrace => depth -= 1,
            _ => {}
        }
    }
    depth <= 0
}

/// Index of the first token of the receiver expression that ends `toks`.
fn expr_start(toks: &[Token]) -> usize {
    let mut i = toks.len();
    loop {
        let Some(j) = operand_start(toks, i) else { return i };
        i = j;
        if i > 0 && matches!(toks[i - 1].kind, K::Dot | K::Arrow | K::ColonColon) {
            i -= 1;
        } else {
            return i;
        }
    }
}

/// Start of `name[<args>]` followed by any call/subscript groups, ending at `end`.
fn operand_start(toks: &[Token], end: usize) -> Option<usize> {
    let mut i = end;
    let mut paren = false;
    while i > 0 && matches!(toks[i - 1].kind, K::RParen | K::RBracket | K::Gt) {
        paren = toks[i - 1].kind == K::RParen;
        i = matching_back(toks, i - 1)?;
    }
    if i > 0 && toks[i - 1].kind == K::Ident {
        Some(i - 1)
    } else if paren {
        Some(i) // a parenthesized primary: `(*p).`
    } else {
        None
    }
}

/// `toks` split on commas outside any brackets (angle brackets included).
fn split_call_args(toks: &[Token]) -> Vec<&[Token]> {
    let (mut out, mut depth, mut start) = (Vec::new(), 0i32, 0);
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            K::LParen | K::LBracket | K::LBrace | K::Lt => depth += 1,
            K::RParen | K::RBracket | K::RBrace | K::Gt => depth = (depth - 1).max(0),
            K::Comma if depth == 0 => {
                out.push(&toks[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < toks.len() {
        out.push(&toks[start..]);
    }
    out
}

/// First index in `toks` of a `kind` token outside any brackets.
fn top_level(toks: &[Token], kind: K) -> Option<usize> {
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            K::LParen | K::LBracket | K::LBrace => depth += 1,
            K::RParen | K::RBracket | K::RBrace => depth -= 1,
            k if k == kind && depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

fn close_of(k: K) -> Option<K> {
    Some(match k {
        K::LParen => K::RParen,
        K::LBracket => K::RBracket,
        K::LBrace => K::RBrace,
        K::Lt => K::Gt,
        _ => return None,
    })
}

fn open_of(k: K) -> Option<K> {
    Some(match k {
        K::RParen => K::LParen,
        K::RBracket => K::LBracket,
        K::RBrace => K::LBrace,
        K::Gt => K::Lt,
        _ => return None,
    })
}

/// Index of the token closing the group opened at `toks[i]`.
pub(super) fn matching_fwd(toks: &[Token], i: usize) -> Option<usize> {
    let open = toks[i].kind;
    let close = close_of(open)?;
    let mut depth = 0i32;
    for (j, t) in toks.iter().enumerate().skip(i) {
        if t.kind == open {
            depth += 1;
        } else if t.kind == close {
            depth -= 1;
            if depth == 0 {
                return Some(j);
            }
        }
    }
    None
}

/// Index of the token opening the group closed at `toks[i]`.
fn matching_back(toks: &[Token], i: usize) -> Option<usize> {
    let close = toks[i].kind;
    let open = open_of(close)?;
    let mut depth = 0i32;
    for j in (0..=i).rev() {
        if toks[j].kind == close {
            depth += 1;
        } else if toks[j].kind == open {
            depth -= 1;
            if depth == 0 {
                return Some(j);
            }
        }
    }
    None
}

/// Type spelled at the end of `toks` in a declaration like
/// `eastl::vector<Foo> *`, reduced to its leading template name and arguments.
fn declared_type_tail(toks: &[Token], text: &str) -> Option<Ty> {
    let mut i = toks.len();
    let mut ptr = 0u8;
    // Out-of-line definitions: `Foo* Bar::baz` leaves `Bar::` before the name.
    while i >= 2 && toks[i - 1].kind == K::ColonColon && toks[i - 2].kind == K::Ident {
        i -= 2;
    }
    while i > 0 && (matches!(toks[i - 1].kind, K::Star | K::Amp | K::AmpAmp) || toks[i - 1].is_keyword(Keyword::Const)) {
        ptr += (toks[i - 1].kind == K::Star) as u8;
        i -= 1;
    }
    let mut args = Vec::new();
    if i > 0 && toks[i - 1].kind == K::Gt {
        let open = matching_back(toks, i - 1)?;
        args = split_args(&toks[open + 1..i - 1], text);
        i = open;
    }
    // `conditional<B, X, Y>::type`: the trait's result.
    if i >= 3 && args.is_empty() && toks[i - 1].kind == K::Ident && toks[i - 1].text(text) == "type" && toks[i - 2].kind == K::ColonColon && toks[i - 3].kind == K::Gt {
        let inner = declared_type_tail(&toks[..i - 2], text)?;
        return Some(trait_result(&inner).unwrap_or(inner));
    }
    let name = (i > 0 && toks[i - 1].kind == K::Ident).then(|| i - 1)?;
    let (mut lo, mut path) = (name, Vec::new());
    while lo >= 2 && toks[lo - 1].kind == K::ColonColon && toks[lo - 2].kind == K::Ident {
        path.push(toks[lo - 2].text(text));
        lo -= 2;
    }
    path.reverse();
    let rooted = lo >= 1 && toks[lo - 1].kind == K::ColonColon && (lo == 1 || !matches!(toks[lo - 2].kind, K::Gt | K::Ident));
    let qual = match (rooted, path.is_empty()) {
        (false, _) => path.join("::"),
        (true, true) => "::".to_owned(),
        (true, false) => format!("::{}", path.join("::")),
    };
    let ty = Ty { head: toks[name].text(text).to_owned(), args, qual, ptr };
    Some(trait_result(&ty).unwrap_or(ty))
}

/// What a type-trait application like `conditional_t<B, X, Y>` or
/// `remove_reference<T>` stands for, when the choice doesn't depend on values.
fn trait_result(ty: &Ty) -> Option<Ty> {
    let name = ty.head.strip_suffix("_t").unwrap_or(&ty.head);
    let pick = match name {
        "conditional" => 2,
        "enable_if" => 1,
        "remove_reference" | "remove_const" | "remove_cv" | "remove_volatile" | "remove_pointer" | "remove_cvref" | "decay" | "add_pointer" | "add_const" | "add_lvalue_reference"
        | "add_rvalue_reference" | "type_identity" | "identity" => 0,
        _ => return None,
    };
    parse_ty(ty.args.get(pick)?)
}

/// The type written in `s`, e.g. `vector<Foo>`.
fn parse_ty(s: &str) -> Option<Ty> {
    let toks = sig_tokens(s);
    declared_type_tail(&toks, s)
}

/// Template arguments (the tokens between `<` and `>`) as normalized type
/// texts; an argument that isn't a type (`3`, `sizeof(T)`) becomes empty so
/// positions stay aligned with the parameters.
pub(super) fn split_args(toks: &[Token], text: &str) -> Vec<String> {
    // Pointer-ness is kept (`Foo*`) so partial specializations like `X<T*>` can be told apart.
    let norm = |seg: &[Token]| {
        let stars = seg.iter().rev().take_while(|t| matches!(t.kind, K::Star | K::Amp | K::AmpAmp) || t.is_keyword(Keyword::Const)).filter(|t| t.kind == K::Star).count();
        declared_type_tail(seg, text).map(|t| t.text() + &"*".repeat(stars)).unwrap_or_default()
    };
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            K::Lt | K::LParen | K::LBracket | K::LBrace => depth += 1,
            K::Gt | K::RParen | K::RBracket | K::RBrace => depth -= 1,
            K::Comma if depth == 0 => {
                out.push(norm(&toks[start..i]));
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < toks.len() {
        out.push(norm(&toks[start..]));
    }
    out
}

/// Where the range expression of a range-for that starts at `from` ends: the
/// closing paren of the `for (`.
fn range_end(toks: &[Token], from: usize) -> usize {
    let mut depth = 0i32;
    for (j, t) in toks.iter().enumerate().skip(from) {
        match t.kind {
            K::LParen | K::LBracket | K::LBrace => depth += 1,
            K::RParen if depth == 0 => return j,
            K::RParen | K::RBracket | K::RBrace => depth -= 1,
            _ => {}
        }
    }
    toks.len()
}

/// Where the initializer starting at `from` ends: the next top-level `;`.
fn init_end(toks: &[Token], from: usize) -> usize {
    let mut depth = 0i32;
    for (j, t) in toks.iter().enumerate().skip(from) {
        match t.kind {
            K::LParen | K::LBracket | K::LBrace => depth += 1,
            K::RParen | K::RBracket | K::RBrace => depth -= 1,
            K::Semi if depth <= 0 => return j,
            _ => {}
        }
    }
    toks.len()
}

/// The `[`..`]` of the structured binding `auto& [a, b]` that names `toks[i]`.
fn binding_group(toks: &[Token], text: &str, i: usize) -> Option<(usize, usize)> {
    let mut lb = i;
    loop {
        lb = lb.checked_sub(1)?;
        match toks[lb].kind {
            K::Ident | K::Comma => {}
            K::LBracket => break,
            _ => return None,
        }
    }
    let mut k = lb;
    while k > 0 && (matches!(toks[k - 1].kind, K::Star | K::Amp | K::AmpAmp) || toks[k - 1].is_keyword(Keyword::Const)) {
        k -= 1;
    }
    if k == 0 || toks[k - 1].kind != K::Ident || toks[k - 1].text(text) != "auto" {
        return None;
    }
    Some((lb, matching_fwd(toks, lb).filter(|&rb| rb > i)?))
}

/// Finds the nearest earlier declaration `Type [*&] name` of `name` in `toks`
/// (tokens of `text` before the use).
fn declared_before(toks: &[Token], text: &str, name: &str) -> Option<Decl> {
    for i in (1..toks.len()).rev() {
        let t = &toks[i];
        if t.kind != K::Ident || t.text(text) != name {
            continue;
        }
        let follows = toks.get(i + 1).is_none_or(|n| matches!(n.kind, K::Semi | K::Eq | K::LParen | K::LBrace | K::Comma | K::RParen | K::LBracket | K::RBracket | K::Colon));
        if !follows {
            continue;
        }
        if let Some((lb, rb)) = binding_group(toks, text, i) {
            let n = toks[lb + 1..i].iter().filter(|t| t.kind == K::Ident).count();
            let start = rb + 2;
            let inner = match toks.get(rb + 1).map(|t| t.kind) {
                Some(K::Colon) => Decl::Range { decl: lb, start, end: range_end(toks, start) },
                Some(K::Eq) => Decl::Auto { decl: lb, start, end: init_end(toks, start) },
                _ => continue,
            };
            return Some(Decl::Bind(n, Box::new(inner)));
        }
        let Some(ty) = declared_type_tail(&toks[..i], text) else { continue };
        if ty.head != "auto" {
            return Some(Decl::Ty(ty));
        }
        match toks.get(i + 1).map(|n| n.kind) {
            Some(K::Colon) => return Some(Decl::Range { decl: i, start: i + 2, end: range_end(toks, i + 2) }),
            Some(K::Eq) => return Some(Decl::Auto { decl: i, start: i + 2, end: init_end(toks, i + 2) }),
            _ => {}
        }
    }
    None
}
