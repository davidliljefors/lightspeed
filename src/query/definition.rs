//! Go-to-definition: resolves the identifier under the cursor to the declarations it may name.

use super::{Decls, Query, call_arity, drop_forward_declarations, ident_at, is_forward_decl_kind, sig_tokens, types};
use crate::base::ids::Idx;
use crate::lexer::{Token, TokenKind};
use crate::symbols::{SymbolFlags, SymbolId};

impl Query<'_> {
    /// Declarations the identifier at `offset` may refer to, best first
    /// (all top-scoring candidates). Out-of-line members resolve to their
    /// definitions when one exists, otherwise to the declaration.
    pub fn definition(&self, path: &str, text: &str, offset: u32) -> Vec<SymbolId> {
        let toks = sig_tokens(text);
        let Some(i) = ident_at(&toks, offset) else { return Vec::new() };
        self.definition_in(&toks, path, text, i)
    }

    /// [`Self::definition`] for the identifier token `toks[i]` of an already lexed file.
    pub(super) fn definition_in(&self, toks: &[Token], path: &str, text: &str, i: usize) -> Vec<SymbolId> {
        self.resolve_identifier(toks, path, text, i, &|_| true, &mut false, true)
    }

    /// [`Self::definition_in`] considering only symbols whose kind satisfies `kind_ok`.
    pub(super) fn definition_of_kind(&self, toks: &[Token], path: &str, text: &str, i: usize, kind_ok: &dyn Fn(crate::symbols::SymbolKind) -> bool) -> Vec<SymbolId> {
        self.resolve_identifier(toks, path, text, i, kind_ok, &mut false, false)
    }

    /// [`Self::definition_in`] that also says whether it had to guess: `guessed` is set for a member
    /// access whose receiver type is unknown, which then matches every member of that name.
    pub(super) fn definition_checked(&self, toks: &[Token], path: &str, text: &str, i: usize) -> (Vec<SymbolId>, bool) {
        let mut guessed = false;
        super::types::take_guess();
        let r = self.resolve_identifier(toks, path, text, i, &|_| true, &mut guessed, true);
        (r, guessed | super::types::take_guess())
    }

    /// `any_kind` says `kind_ok` accepts everything, which is what lets the unknown-receiver answer be shared.
    fn resolve_identifier(&self, toks: &[Token], path: &str, text: &str, i: usize, kind_ok: &dyn Fn(crate::symbols::SymbolKind) -> bool, guessed: &mut bool, any_kind: bool) -> Vec<SymbolId> {
        let offset = toks[i].start;
        let name = toks[i].text(text);
        let (db, hier) = (&self.idx.db, &self.idx.hier);

        let member = i > 0 && matches!(toks[i - 1].kind, TokenKind::Dot | TokenKind::Arrow);
        let mut qual: Vec<&str> = Vec::new();
        let mut j = i;
        while j >= 2 && toks[j - 1].kind == TokenKind::ColonColon && toks[j - 2].kind == TokenKind::Ident {
            qual.push(toks[j - 2].text(text));
            j -= 2;
        }
        qual.reverse();
        let global = j >= 1 && toks[j - 1].kind == TokenKind::ColonColon;
        if j == i && i >= 2 && toks[i - 1].kind == TokenKind::ColonColon && matches!(toks[i - 2].kind, TokenKind::Gt | TokenKind::RParen | TokenKind::RBracket) {
            // `Tmpl<T>::name`: the qualifier is a type we do not follow, so it cannot be told from a bare name.
            *guessed = true;
        }
        let arity = (toks.get(i + 1).is_some_and(|t| t.kind == TokenKind::LParen)).then(|| call_arity(toks, i + 1));

        use super::refs::PROF;
        use std::sync::atomic::Ordering::Relaxed;
        let tp = std::time::Instant::now();
        PROF[7].fetch_add(1, Relaxed);
        let fi = self.idx.file_index(path);
        let scopes = self.scopes_at(fi, offset);
        let (in_scope, in_bases) = self.scope_set(&scopes);
        PROF[8].fetch_add(tp.elapsed().as_nanos() as u64, Relaxed);

        let elaborated = i > 0 && matches!(toks[i - 1].keyword, crate::lexer::Keyword::Class | crate::lexer::Keyword::Struct | crate::lexer::Keyword::Union | crate::lexer::Keyword::Enum);
        let name_id = db.names.lookup(name);
        let same_name = || name_id.map_or(&[][..], |n| db.lookup_id(n));
        let keep = |c: SymbolId| kind_ok(db.symbol(c).kind) && (!elaborated || db.symbol(c).kind.is_type());
        let all_named = || -> Vec<SymbolId> {
            let mut v: Vec<SymbolId> = same_name().iter().map(|&c| hier.canonical(c)).filter(|&c| keep(c)).collect();
            v.sort_unstable();
            v.dedup();
            v
        };

        // A popular name (`copyFields`) has thousands of same-named symbols, so the bucket by name is only walked
        // when nothing narrower is known: a receiver type or written qualifier is looked up through its scope instead.
        let mut cands: Vec<SymbolId> = Vec::new();
        let mut narrowed = false;
        let mut shared_key = None;
        // Template arguments after the name would change which specialization is picked.
        let shareable = any_kind && toks.get(i + 1).is_none_or(|t| t.kind != TokenKind::Lt);
        if member {
            PROF[11].fetch_add(1, Relaxed);
            let tp = std::time::Instant::now();
            let recv = toks[i - 1].start;
            let types = self.receiver_types_in(path, text, &toks[..i - 1], recv as usize, &scopes);
            PROF[10].fetch_add(tp.elapsed().as_nanos() as u64, Relaxed);
            *guessed = types.is_empty();
            if types.is_empty() && shareable && qual.is_empty()
                && let Some(n) = name_id
            {
                let key = (n, arity, true);
                if let Some(hit) = self.unknown_receiver.lock().unwrap().get(&key) {
                    return hit.to_vec();
                }
                shared_key = Some(key);
            }
            if !types.is_empty() {
                let mut owners = Vec::new();
                for &t in &types {
                    owners.push(t);
                    hier.all_bases(t, &mut owners);
                }
                if let Some(n) = name_id {
                    cands = owners
                        .iter()
                        .flat_map(|&o| db.children(o).iter().copied())
                        .filter(|&c| db.symbol(c).name == n)
                        .map(|c| hier.canonical(c))
                        .filter(|&c| keep(c) && db.symbol(c).parent.is_some_and(|p| owners.contains(&p)))
                        .collect();
                    cands.sort_unstable();
                    cands.dedup();
                }
                if cands.is_empty() {
                    *guessed = true;
                } else {
                    narrowed = true;
                }
            }
        }
        if !narrowed && qual.is_empty() {
            let tp = std::time::Instant::now();
            cands = all_named();
            PROF[9].fetch_add(tp.elapsed().as_nanos() as u64, Relaxed);
        }
        PROF[12].fetch_add(cands.len() as u64, Relaxed);
        let mut lt = std::time::Instant::now();
        super::refs::lap(14, &mut lt);
        PROF[24].fetch_add(cands.len() as u64, Relaxed);
        if !qual.is_empty() {
            PROF[25].fetch_add(1, Relaxed);
        }

        // Computed on first use: most lookups never need them.
        let decls = std::cell::OnceCell::new();
        let decls_at = || -> &Decls {
            decls.get_or_init(|| Decls {
                local: crate::scope_decls::scan(toks, text, i, false),
                included: fi.map(|f| self.idx.included_decls(f)),
            })
        };

        if global && qual.is_empty() {
            let top: Vec<SymbolId> = cands.iter().copied().filter(|&c| db.symbol(c).parent.is_none()).collect();
            if !top.is_empty() {
                cands = top;
            }
        }
        if !qual.is_empty() {
            let mut buf = String::new();
            let enclosing: Vec<String> = scopes.iter().map(|&s| db.qualified_name(s)).collect();
            let seed: Option<Vec<SymbolId>> = narrowed.then(|| cands.clone());
            let mut by_qualifier = |want: &str| -> Vec<SymbolId> {
                // Only a symbol whose parent is called like the last written component can match, so that is all we walk.
                let under: Vec<SymbolId> = match &seed {
                    Some(s) => s.clone(),
                    None => match (name_id, db.names.lookup(want.rsplit("::").next().unwrap_or(want))) {
                        (Some(n), Some(q)) => db.lookup_id(n).iter().map(|&c| hier.canonical(c)).filter(|&c| keep(c) && db.symbol(c).parent.is_some_and(|p| db.symbol(p).name == q)).collect(),
                        _ => Vec::new(),
                    },
                };
                // (length of the namespace prefix before the written qualifier, whether an enclosing scope spells it)
                let mut hits: Vec<(SymbolId, usize, bool)> = Vec::new();
                for &c in &under {
                    let Some(p) = db.symbol(c).parent else { continue };
                    buf.clear();
                    db.write_qualified_name(p, &mut buf);
                    if buf == want {
                        hits.push((c, 0, true));
                    } else if !global && let Some(r) = buf.strip_suffix(want).and_then(|r| r.strip_suffix("::")) {
                        hits.push((c, r.len() + 2, enclosing.iter().any(|e| e == r)));
                    }
                }
                // `X::` written inside `n` means `n::X` (innermost wins); from outside it means the global `X`,
                // and only when nothing else is spelled that way may a `using namespace` reach a deeper one.
                let valid = hits.iter().filter(|h| h.2).map(|h| h.1).max();
                match valid {
                    Some(len) => hits.iter().filter(|h| h.2 && h.1 == len).map(|h| h.0).collect(),
                    None => hits.iter().map(|h| h.0).collect(),
                }
            };
            let mut filtered = by_qualifier(&qual.join("::"));
            if filtered.is_empty()
                && let Some(target) = decls_at().alias(qual[0])
            {
                // `namespace fs = std::filesystem;` makes `fs::` mean `std::filesystem::`.
                let mut want = target.to_owned();
                for q in &qual[1..] {
                    want.push_str("::");
                    want.push_str(q);
                }
                filtered = by_qualifier(&want);
            }
            if filtered.is_empty() {
                // `using RD = RenderingDevice;` makes `RD::` mean `RenderingDevice::`. A class's own alias
                // (`using Super_t = Base;`) is found through the scope we are in before any other of that name.
                let nested: Vec<SymbolId> = db
                    .names
                    .lookup(qual[0])
                    .map(|n| in_scope.iter().chain(&in_bases).flat_map(|&s| db.children(s).iter().copied()).filter(|&c| db.symbol(c).name == n && db.symbol(c).kind == crate::symbols::SymbolKind::TypeAlias).collect())
                    .unwrap_or_default();
                let aliases: Vec<SymbolId> = if nested.is_empty() {
                    db.lookup(qual[0]).iter().copied().filter(|&c| db.symbol(c).kind == crate::symbols::SymbolKind::TypeAlias).collect()
                } else {
                    nested
                };
                let mut targets = self.resolve_qualifiers(aliases);
                for &m in db.lookup(qual[0]).iter().filter(|&&c| db.symbol(c).kind == crate::symbols::SymbolKind::Macro) {
                    if let Some(t) = self.object_macro_target(m) {
                        let last = t.rsplit("::").next().unwrap_or(&t);
                        let named: Vec<SymbolId> = db.lookup(last).iter().copied().filter(|&c| db.symbol(c).kind.is_type()).collect();
                        targets.extend(self.resolve_records(named));
                    }
                }
                for rec in targets {
                    let mut want = db.qualified_name(rec);
                    for q in &qual[1..] {
                        want.push_str("::");
                        want.push_str(q);
                    }
                    filtered.extend(by_qualifier(&want));
                }
                filtered.sort_unstable();
                filtered.dedup();
            }
            if filtered.is_empty() {
                // Nothing lives under the written qualifier, so any survivor is a same-named stranger.
                *guessed = true;
                if !narrowed {
                    if shareable
                        && !member
                        && let Some(n) = name_id
                    {
                        let key = (n, arity, false);
                        if let Some(hit) = self.unknown_receiver.lock().unwrap().get(&key) {
                            return hit.to_vec();
                        }
                        shared_key = Some(key);
                    }
                    cands = all_named();
                }
            } else {
                filtered.sort_unstable();
                filtered.dedup();
                cands = filtered;
            }
        }

        super::refs::lap(15, &mut lt);
        drop_forward_declarations(db, &mut cands);
        super::refs::lap(16, &mut lt);
        PROF[26].fetch_add(cands.len() as u64, Relaxed);
        if cands.iter().any(|&c| db.symbol(c).flags.contains(crate::symbols::SymbolFlags::SPECIALIZATION)) {
            let args = (toks.get(i + 1).is_some_and(|t| t.kind == TokenKind::Lt))
                .then(|| types::matching_fwd(toks, i + 1))
                .flatten()
                .map(|c| types::split_args(&toks[i + 2..c], text));
            self.choose_specialization(&mut cands, args.as_deref());
        }
        super::refs::lap(17, &mut lt);
        if !member && qual.is_empty() && !(i > 0 && toks[i - 1].kind == TokenKind::ColonColon) {
            PROF[27].fetch_add(1, Relaxed);
            cands.retain(|&c| db.symbol(c).kind.is_callable() || self.visible_unqualified(c, &in_scope, &in_bases, || decls_at()));
            // A bare name never reaches a method of a class we are not inside of when something else can be meant.
            let in_class = |c: SymbolId| db.symbol(c).parent.is_some_and(|p| db.symbol(p).kind.is_record());
            let reachable = |c: SymbolId| db.symbol(c).parent.is_some_and(|p| in_scope.iter().any(|&x| self.same_scope(x, p)) || in_bases.contains(&p));
            let kept: Vec<SymbolId> = cands.iter().copied().filter(|&c| !(db.symbol(c).kind.is_callable() && in_class(c) && !reachable(c))).collect();
            if !kept.is_empty() {
                cands = kept;
            }
        }

        if arity.is_none() && !member && qual.is_empty() && cands.iter().any(|&c| db.symbol(c).kind.is_record()) {
            // Without a call, a class name means the class, not its constructors.
            let is_ctor = |c: SymbolId| {
                let s = db.symbol(c);
                s.kind.is_callable() && s.parent.is_some_and(|p| db.symbol(p).kind.is_record() && db.symbol(p).name == s.name)
            };
            cands.retain(|&c| !is_ctor(c));
        }

        super::refs::lap(18, &mut lt);
        PROF[28].fetch_add(cands.len() as u64, Relaxed);
        let mut best: (i32, Vec<SymbolId>) = (i32::MIN, Vec::new());
        for &c in &cands {
            let s = db.symbol(c);
            let mut score = 0i32;
            let parent_kind = s.parent.map(|p| db.symbol(p).kind);
            let is_member = parent_kind.is_some_and(|k| k.is_record());
            if member {
                score += if is_member { 4 } else { -6 };
            } else if qual.is_empty() {
                if let Some(p) = s.parent {
                    if let Some(pos) = in_scope.iter().position(|&x| self.same_scope(x, p)) {
                        score += 8 - pos.min(4) as i32;
                    } else if in_bases.contains(&p) {
                        score += 3;
                    } else if db.symbol(p).kind == crate::symbols::SymbolKind::Namespace && decls_at().names_contain(db.names.get(db.symbol(p).name)) {
                        // Named by a `using namespace` in effect here.
                        score += 5;
                    }
                }
                if s.kind == crate::symbols::SymbolKind::EnumConstant
                    && let Some(p) = s.parent.filter(|&p| db.symbol(p).flags.contains(SymbolFlags::SCOPED))
                    && !in_scope.contains(&p)
                {
                    // `enum class` constants need their qualifier.
                    score -= 10;
                }
                if fi.is_some_and(|f| self.idx.files[f].syms.0 <= c.index() as u32 && (c.index() as u32) < self.idx.files[f].syms.1) {
                    score += 1;
                }
            }
            if let Some(n) = arity {
                match db.signature(c) {
                    Some(sig) if u32::from(sig.arity) == n => score += 3,
                    Some(sig) if u32::from(sig.arity) > n => score += 1,
                    Some(_) => score -= 5,
                    None if s.kind.is_record() => score += 1,
                    None => score -= 2,
                }
                if s.kind.is_callable() || s.kind.is_record() {
                    score += 1;
                }
            }
            if score > best.0 {
                best = (score, vec![c]);
            } else if score == best.0 {
                best.1.push(c);
            }
        }

        super::refs::lap(19, &mut lt);
        let mut picked = best.1;
        PROF[29].fetch_add(picked.len() as u64, Relaxed);
        let same_class = picked.iter().all(|&c| db.symbol(c).parent == db.symbol(picked[0]).parent);
        let mut by_overloads = false;
        if arity.is_some() && picked.len() > 1 && same_class && picked.iter().all(|&c| db.symbol(c).kind.is_callable()) {
            picked = self.best_overloads(path, text, toks, i, picked);
            by_overloads = true;
        }
        super::refs::lap(20, &mut lt);
        if picked.iter().any(|&c| is_forward_decl_kind(db, c) && db.symbol(c).flags.contains(SymbolFlags::DEFINITION)) {
            picked.retain(|&c| !is_forward_decl_kind(db, c) || db.symbol(c).flags.contains(SymbolFlags::DEFINITION));
        }
        let mut out = Vec::new();
        for c in picked {
            let defs = hier.out_of_line_defs(c);
            if defs.is_empty() || db.symbol(c).flags.contains(SymbolFlags::DEFINITION) {
                out.push(c);
            } else {
                out.extend_from_slice(defs);
            }
        }
        if let Some(key) = shared_key
            && !by_overloads
        {
            self.unknown_receiver.lock().unwrap().insert(key, std::sync::Arc::new(out.clone()));
        }
        out
    }

    /// Whether `a` and `b` are the same scope; reopened namespaces are separate
    /// symbols, so those compare by their chain of names.
    pub(super) fn same_scope(&self, mut a: SymbolId, mut b: SymbolId) -> bool {
        use crate::symbols::SymbolKind as K;
        let db = &self.idx.db;
        loop {
            if a == b {
                return true;
            }
            let (sa, sb) = (db.symbol(a), db.symbol(b));
            if sa.kind != K::Namespace || sb.kind != K::Namespace || sa.name != sb.name {
                return false;
            }
            match (sa.parent, sb.parent) {
                (None, None) => return true,
                (Some(pa), Some(pb)) => (a, b) = (pa, pb),
                _ => return false,
            }
        }
    }

    /// Whether `c` can be named without qualification from a scope whose
    /// enclosing scopes are `in_scope` (and inherited ones `in_bases`).
    /// Members of other namespaces and classes need a qualifier or a `using`.
    pub(super) fn visible_unqualified<'t>(&self, c: SymbolId, in_scope: &[SymbolId], in_bases: &[SymbolId], decls: impl FnOnce() -> &'t Decls) -> bool {
        use crate::symbols::SymbolKind as K;
        let db = &self.idx.db;
        // Unscoped enumerators and anonymous namespaces belong to the enclosing scope.
        let mut parent = db.symbol(c).parent;
        while let Some(p) = parent {
            let s = db.symbol(p);
            let transparent = (s.kind == K::Enum && !s.flags.contains(SymbolFlags::SCOPED)) || (s.kind == K::Namespace && db.names.get(s.name).is_empty());
            if !transparent {
                break;
            }
            parent = s.parent;
        }
        let Some(p) = parent else { return true };
        let kind = db.symbol(p).kind;
        if !matches!(kind, K::Namespace | K::Enum) && !kind.is_record() {
            return true;
        }
        if in_scope.iter().any(|&s| self.same_scope(s, p)) || in_bases.contains(&p) {
            return true;
        }
        let name = db.names.get(db.symbol(p).name);
        decls().names_contain(name)
    }
}
