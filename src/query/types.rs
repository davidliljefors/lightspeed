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
//! * `auto x = <expr>;` by evaluating the initializer (depth-limited);
//! * smart pointers: `->` on `unique_ptr<T>` and friends yields `T`.
//!
//! Aliases are chased through their written target. Everything is best
//! effort and returns "unknown" (an empty list) rather than guessing; callers
//! then fall back to name-based matching.
//!
//! This is the one place to extend type inference.

use super::{Query, sig_tokens};
use crate::lexer::{Keyword, Token, TokenKind as K};
use crate::symbols::{SymbolId, SymbolKind};

const MAX_DEPTH: u8 = 4;
const SMART_PTRS: &[&str] = &["unique_ptr", "shared_ptr", "weak_ptr", "intrusive_ptr", "scoped_ptr", "linked_ptr", "optional"];
const MAKE_SMART: &[&str] = &["make_unique", "make_shared", "make_intrusive", "allocate_shared", "make_optional"];
/// Calls whose result has the type of their first argument.
const PASS_THROUGH: &[&str] = &["move", "forward", "as_const", "addressof", "ref", "cref"];

/// A written type reduced to what member lookup needs: its leading name and,
/// for templates, the first type argument.
#[derive(Clone)]
struct Ty {
    head: String,
    arg: Option<String>,
}

impl Ty {
    fn plain(head: &str) -> Self {
        Self { head: head.to_owned(), arg: None }
    }

    /// The type reached through `->` or `*` on a smart pointer.
    fn smart_inner(&self) -> Option<Self> {
        match &self.arg {
            Some(a) if SMART_PTRS.contains(&self.head.as_str()) => Some(Self::plain(a)),
            _ => None,
        }
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
    /// `auto name = <init>`: the declaration's token index and the end of its initializer.
    Auto { decl: usize, end: usize },
    /// `auto name : <range>` in a range-for.
    Range { decl: usize, end: usize },
}

impl Query<'_> {
    /// Records `ids` may denote, following type aliases (`typedef struct _X X`,
    /// `using X = Y`) through their written target. Depth-limited.
    pub(super) fn resolve_records(&self, ids: impl IntoIterator<Item = SymbolId>) -> Vec<SymbolId> {
        let db = &self.idx.db;
        let mut out = Vec::new();
        let mut work: Vec<(SymbolId, u8)> = ids.into_iter().map(|i| (self.idx.hier.canonical(i), 0)).collect();
        while let Some((id, depth)) = work.pop() {
            match db.symbol(id).kind {
                k if k.is_record() => out.push(id),
                SymbolKind::TypeAlias if depth < 6 => {
                    if let Some(t) = self.alias_target(id) {
                        work.extend(db.lookup(&t).iter().filter(|&&c| c != id).map(|&c| (c, depth + 1)));
                    }
                }
                _ => {}
            }
        }
        out.sort_unstable();
        out.dedup();
        out
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
        self.resolve_records(db.lookup(&ty.head).iter().copied())
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
        while let Some(t) = expr.get(start).filter(|t| matches!(t.kind, K::Star | K::Amp | K::AmpAmp) || t.is_keyword(Keyword::Const)) {
            derefs += (t.kind == K::Star) as usize;
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
                    if expr.get(i).is_some_and(|t| t.kind == K::LParen) {
                        i = matching_fwd(expr, i)? + 1;
                    }
                    ty = self.operator_ty(cx, &recv, cx.text_of(n))?;
                }
                K::LBracket => {
                    i = matching_fwd(expr, i)? + 1;
                    ty = match self.operator_ty(cx, &ty, "operator[]").filter(|t| self.resolves(t)) {
                        Some(t) => t,
                        None => ty.arg.take().map_or(ty, |a| Ty::plain(&a)),
                    };
                }
                _ => break,
            }
        }
        for _ in 0..derefs {
            ty = self.deref_ty(cx, ty);
        }
        let Some(t) = expr.get(i) else { return Some(ty) };
        match t.kind {
            K::Eq | K::Comma | K::Semi => Some(ty),
            // Binary operator: only overloaded ones on class types yield members.
            _ => self.operator_ty(cx, &ty, &format!("operator{}", cx.text_of(t))),
        }
    }

    /// A primary expression at `expr[i..]` and the index just after it.
    fn primary_ty(&self, cx: &Cx, prefix: &[Token], expr: &[Token], mut i: usize, depth: u8) -> Option<(Ty, usize)> {
        let tx = |t: &Token| t.text(cx.text);
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
        let mut targ: Option<&str> = None;
        loop {
            let t = expr.get(i).filter(|t| t.kind == K::Ident)?;
            path.push(tx(t));
            i += 1;
            if expr.get(i).is_some_and(|t| t.kind == K::Lt) {
                let close = matching_fwd(expr, i)?;
                targ = targ.or_else(|| first_type_name(&expr[i + 1..close], cx.text));
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
            Ty { head: name.to_owned(), arg: targ.map(str::to_owned) }
        } else if called && PASS_THROUGH.contains(&name) {
            let args = &expr[open + 1..i - 1];
            let first = &args[..top_level(args, K::Comma).unwrap_or(args.len())];
            self.expr_ty(cx, prefix, first, depth + 1)?
        } else if called {
            self.call_ty(cx, path.last().copied(), name, targ)?
        } else if let Some(q) = path.last() {
            let owners = self.resolve_records(self.idx.db.lookup(q).iter().copied());
            self.member_ty(cx, &owners, name)?
        } else {
            self.ident_ty(cx, prefix, name, depth)?
        };
        Some((ty, i))
    }

    fn resolves(&self, ty: &Ty) -> bool {
        !self.resolve_records(self.idx.db.lookup(&ty.head).iter().copied()).is_empty()
    }

    /// Member `name` (usually an `operator…`) of the class `ty` names, or its bases.
    fn operator_ty(&self, cx: &Cx, ty: &Ty, name: &str) -> Option<Ty> {
        let owners = self.resolve_records(self.idx.db.lookup(&ty.head).iter().copied());
        self.member_ty(cx, &owners, name)
    }

    /// The type reached through `->`.
    fn arrow_ty(&self, cx: &Cx, ty: Ty) -> Ty {
        if let Some(t) = ty.smart_inner() {
            return t;
        }
        self.operator_ty(cx, &ty, "operator->").filter(|t| self.resolves(t)).unwrap_or(ty)
    }

    /// The type of `*expr`.
    fn deref_ty(&self, cx: &Cx, ty: Ty) -> Ty {
        if let Some(t) = ty.smart_inner() {
            return t;
        }
        self.operator_ty(cx, &ty, "operator*").filter(|t| self.resolves(t)).unwrap_or(ty)
    }

    /// Element type when iterating over a value of type `ty` in a range-for.
    fn element_ty(&self, cx: &Cx, ty: Ty) -> Option<Ty> {
        if ty.head.contains("map") {
            return None; // elements are pairs; not modelled
        }
        if let Some(a) = &ty.arg {
            return Some(Ty::plain(a));
        }
        match self.operator_ty(cx, &ty, "begin") {
            Some(it) => Some(self.deref_ty(cx, it)),
            None => Some(ty), // plain C array
        }
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
        Some(match ty.arg {
            Some(a) => format!("{}<{a}>", ty.head),
            None => ty.head,
        })
    }

    /// Type of the variable, parameter, field or `this` called `name`.
    fn ident_ty(&self, cx: &Cx, prefix: &[Token], name: &str, depth: u8) -> Option<Ty> {
        let db = &self.idx.db;
        if name == "this" {
            let c = cx.scopes.iter().copied().find(|&c| db.symbol(c).kind.is_record())?;
            return Some(Ty::plain(db.name(c)));
        }
        match declared_before(prefix, cx.text, name) {
            Some(Decl::Ty(t)) => return Some(t),
            Some(Decl::Auto { decl, end }) => return self.expr_ty(cx, &prefix[..decl], &prefix[decl + 2..end], depth + 1),
            Some(Decl::Range { decl, end }) => {
                let container = self.expr_ty(cx, &prefix[..decl], &prefix[decl + 2..end], depth + 1)?;
                return self.element_ty(cx, container);
            }
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

    /// Result type of calling `name` (optionally qualified by `qual`).
    fn call_ty(&self, cx: &Cx, qual: Option<&str>, name: &str, targ: Option<&str>) -> Option<Ty> {
        if name.ends_with("_cast") {
            return targ.map(Ty::plain);
        }
        if MAKE_SMART.contains(&name) {
            return targ.map(|a| Ty { head: "unique_ptr".to_owned(), arg: Some(a.to_owned()) });
        }
        let db = &self.idx.db;
        if !self.resolve_records(db.lookup(name).iter().copied()).is_empty() {
            return Some(Ty::plain(name));
        }
        let (in_scope, in_bases) = self.scope_set(cx.scopes);
        let local = in_scope.into_iter().chain(in_bases).flat_map(|o| db.children(o).iter().copied());
        local
            .chain(db.lookup(name).iter().copied())
            .filter(|&c| db.name(c) == name && matches!(db.symbol(c).kind, SymbolKind::Function | SymbolKind::Method))
            .filter(|&c| qual.is_none_or(|q| db.symbol(c).parent.is_some_and(|p| db.name(p) == q)))
            .find_map(|c| self.decl_ty(cx, c))
    }

    /// Type of member `name` (field type or method return type) of `owners` or their bases.
    fn member_ty(&self, cx: &Cx, owners: &[SymbolId], name: &str) -> Option<Ty> {
        let db = &self.idx.db;
        let mut chain = Vec::new();
        for &o in owners {
            chain.push(o);
            self.idx.hier.all_bases(o, &mut chain);
        }
        chain
            .iter()
            .flat_map(|&o| db.children(o).iter().copied())
            .filter(|&m| db.name(m) == name && matches!(db.symbol(m).kind, SymbolKind::Field | SymbolKind::Variable | SymbolKind::Function | SymbolKind::Method))
            .find_map(|m| self.decl_ty(cx, m))
    }

    /// Written type of a declaration: a field/variable's type or a function's return type.
    fn decl_ty(&self, cx: &Cx, id: SymbolId) -> Option<Ty> {
        let db = &self.idx.db;
        let s = db.symbol(id);
        let fpath = db.files.path(s.file);
        let t = if fpath == cx.path { None } else { self.texts.text(fpath) };
        let src = t.as_ref().map(|t| t.as_str()).unwrap_or(cx.text);
        let slice = src.get(s.extent_start.min(s.name_offset) as usize..s.name_offset as usize)?;
        let toks = sig_tokens(slice);
        declared_type_tail(&toks, slice).filter(|t| t.head != "auto")
    }
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

/// Last segment of the first qualified name in `toks` (a template argument list).
fn first_type_name<'t>(toks: &[Token], text: &'t str) -> Option<&'t str> {
    let mut i = toks.iter().position(|t| t.kind == K::Ident)?;
    while toks.get(i + 1).is_some_and(|t| t.kind == K::ColonColon) && toks.get(i + 2).is_some_and(|t| t.kind == K::Ident) {
        i += 2;
    }
    Some(toks[i].text(text))
}

/// Type spelled at the end of `toks` in a declaration like
/// `eastl::vector<Foo> *`, reduced to its leading template name and first argument.
fn declared_type_tail(toks: &[Token], text: &str) -> Option<Ty> {
    let mut i = toks.len();
    // Out-of-line definitions: `Foo* Bar::baz` leaves `Bar::` before the name.
    while i >= 2 && toks[i - 1].kind == K::ColonColon && toks[i - 2].kind == K::Ident {
        i -= 2;
    }
    while i > 0 && (matches!(toks[i - 1].kind, K::Star | K::Amp | K::AmpAmp) || toks[i - 1].is_keyword(Keyword::Const)) {
        i -= 1;
    }
    let mut arg = None;
    if i > 0 && toks[i - 1].kind == K::Gt {
        let open = matching_back(toks, i - 1)?;
        arg = first_type_name(&toks[open + 1..i - 1], text).map(str::to_owned);
        i = open;
    }
    (i > 0 && toks[i - 1].kind == K::Ident).then(|| Ty { head: toks[i - 1].text(text).to_owned(), arg })
}

/// Finds the nearest earlier declaration `Type [*&] name` of `name` in `toks`
/// (tokens of `text` before the use).
fn declared_before(toks: &[Token], text: &str, name: &str) -> Option<Decl> {
    for i in (1..toks.len()).rev() {
        let t = &toks[i];
        if t.kind != K::Ident || t.text(text) != name {
            continue;
        }
        let follows = toks.get(i + 1).is_none_or(|n| matches!(n.kind, K::Semi | K::Eq | K::LParen | K::LBrace | K::Comma | K::RParen | K::LBracket | K::Colon));
        if !follows {
            continue;
        }
        let Some(ty) = declared_type_tail(&toks[..i], text) else { continue };
        if ty.head != "auto" {
            return Some(Decl::Ty(ty));
        }
        if toks.get(i + 1).is_some_and(|n| n.kind == K::Colon) {
            // Range-for: the range expression runs to the closing paren of the `for (`.
            let mut depth = 0i32;
            let mut end = toks.len();
            for (j, t) in toks.iter().enumerate().skip(i + 2) {
                match t.kind {
                    K::LParen | K::LBracket | K::LBrace => depth += 1,
                    K::RParen if depth == 0 => {
                        end = j;
                        break;
                    }
                    K::RParen | K::RBracket | K::RBrace => depth -= 1,
                    _ => {}
                }
            }
            return Some(Decl::Range { decl: i, end });
        }
        if toks.get(i + 1).is_some_and(|n| n.kind == K::Eq) {
            let mut depth = 0i32;
            let mut end = toks.len();
            for (j, t) in toks.iter().enumerate().skip(i + 2) {
                match t.kind {
                    K::LParen | K::LBracket | K::LBrace => depth += 1,
                    K::RParen | K::RBracket | K::RBrace => depth -= 1,
                    K::Semi if depth <= 0 => {
                        end = j;
                        break;
                    }
                    _ => {}
                }
            }
            return Some(Decl::Auto { decl: i, end });
        }
    }
    None
}
