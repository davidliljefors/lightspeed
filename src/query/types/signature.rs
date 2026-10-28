//! `textDocument/signatureHelp`: the overloads of the call being typed, best
//! match first. With nothing typed the shortest overload leads; every
//! completed argument whose type is known then pulls matching overloads up.

use super::{Cx, Ty, declared_type_tail, matching_back, matching_fwd, split_call_args, top_level};
use crate::lexer::TokenKind as K;
use crate::query::{Query, sig_tokens};
use crate::symbols::{SymbolFlags, SymbolId, SymbolKind};

/// One overload: the text shown and the byte-free (UTF-16) range of each parameter in it.
pub struct SigInfo {
    pub label: String,
    pub params: Vec<(u32, u32)>,
}

pub struct SigHelp {
    /// Best match first, at most the requested number.
    pub signatures: Vec<SigInfo>,
    pub active_param: usize,
    /// Overloads that did not fit in `signatures`.
    pub more: usize,
}

struct Overload {
    id: SymbolId,
    info: SigInfo,
    tys: Vec<Option<Ty>>,
    variadic: bool,
}

const MAX_CANDIDATES: usize = 128;

impl Query<'_> {
    pub fn signature_help(&self, path: &str, text: &str, offset: u32, max: usize) -> Option<SigHelp> {
        let head = text.get(..(offset as usize).min(text.len()))?;
        let toks = sig_tokens(head);

        let mut depth = 0i32;
        let mut open = None;
        for i in (0..toks.len()).rev() {
            match toks[i].kind {
                K::RParen | K::RBracket | K::RBrace => depth += 1,
                K::LParen if depth == 0 => {
                    open = Some(i);
                    break;
                }
                K::LParen => depth -= 1,
                K::LBracket | K::LBrace if depth == 0 => return None,
                K::LBracket | K::LBrace => depth -= 1,
                K::Semi if depth == 0 => return None,
                _ => {}
            }
        }
        let open = open?;
        let mut name_end = open;
        if name_end > 0 && toks[name_end - 1].kind == K::Gt {
            name_end = matching_back(&toks, name_end - 1)?;
        }
        let name_idx = name_end.checked_sub(1)?;
        let nt = &toks[name_idx];
        if nt.kind != K::Ident || nt.keyword == crate::lexer::Keyword::Reserved {
            return None;
        }

        // Completed arguments and the position of the one being typed.
        let (mut d, mut start) = (0i32, open + 1);
        let mut done: Vec<(usize, usize)> = Vec::new();
        for (i, t) in toks.iter().enumerate().skip(open + 1) {
            match t.kind {
                K::LParen | K::LBracket | K::LBrace | K::Lt => d += 1,
                K::RParen | K::RBracket | K::RBrace | K::Gt => d = (d - 1).max(0),
                K::Comma if d == 0 => {
                    done.push((start, i));
                    start = i + 1;
                }
                _ => {}
            }
        }
        let active = done.len();

        let db = &self.idx.db;
        let found = self.definition_in(&toks, path, head, name_idx);
        let name = nt.text(head);
        let overloadable = |c: SymbolId| {
            let s = db.symbol(c);
            matches!(s.kind, SymbolKind::Function | SymbolKind::Method) && !s.flags.contains(SymbolFlags::DELETED)
        };
        let mut cands: Vec<SymbolId> = Vec::new();
        for &f in &found {
            let s = db.symbol(f);
            if s.kind.is_record() {
                cands.extend(db.children(f).iter().copied().filter(|&c| db.symbol(c).kind == SymbolKind::Constructor && !db.symbol(c).flags.contains(SymbolFlags::DELETED)));
            } else if overloadable(f) || s.kind == SymbolKind::Constructor {
                match s.parent {
                    Some(p) => cands.extend(db.children(p).iter().copied().filter(|&c| db.symbol(c).kind == s.kind && db.name(c) == name && !db.symbol(c).flags.contains(SymbolFlags::DELETED))),
                    None => cands.extend(db.lookup(name).iter().copied().filter(|&c| overloadable(c) && db.symbol(c).parent.is_none())),
                }
            }
        }
        cands.sort_unstable();
        cands.dedup();
        cands.truncate(MAX_CANDIDATES);
        if cands.is_empty() {
            return None;
        }

        let scopes = self.scopes_at(self.idx.file_index(path), nt.start);
        let cx = Cx { path, text, scopes: &scopes };
        let typed: Vec<Option<Ty>> = done.iter().map(|&(a, b)| if a < b { self.expr_ty(&cx, &toks[..a], &toks[a..b], 1) } else { None }).collect();

        let mut overloads: Vec<Overload> = Vec::new();
        for c in cands {
            let Some(o) = self.describe(&cx, c) else { continue };
            if !overloads.iter().any(|x| x.info.label == o.info.label) {
                overloads.push(o);
            }
        }
        if overloads.is_empty() {
            return None;
        }
        let rank = |o: &Overload| {
            let n = o.info.params.len();
            let accepts = (active == 0 && n == 0) || o.variadic || active < n;
            let score: i32 = typed.iter().zip(&o.tys).filter_map(|(a, p)| Some(self.arg_fit(a.as_ref()?, p.as_ref()?))).sum();
            (std::cmp::Reverse(accepts), std::cmp::Reverse(score), n, o.id)
        };
        overloads.sort_by_cached_key(rank);
        let more = overloads.len().saturating_sub(max.max(1));
        overloads.truncate(max.max(1));
        Some(SigHelp { signatures: overloads.into_iter().map(|o| o.info).collect(), active_param: active, more })
    }

    fn describe(&self, cx: &Cx, c: SymbolId) -> Option<Overload> {
        let db = &self.idx.db;
        let s = db.symbol(c);
        let name = db.name(c);
        self.source_of(cx, s, |src| {
            let from = s.name_offset as usize;
            let head = src.get(from..(from + 2000).min(s.extent_end as usize).min(src.len()))?;
            let toks = sig_tokens(head);
            let open = toks.iter().position(|t| t.kind == K::LParen)?;
            let close = matching_fwd(&toks, open)?;
            let inner = &toks[open + 1..close];
            let mut label = format!("{name}(");
            let (mut params, mut tys) = (Vec::new(), Vec::new());
            let mut variadic = false;
            if !(inner.is_empty() || (inner.len() == 1 && inner[0].text(head) == "void")) {
                for (i, p) in split_call_args(inner).into_iter().enumerate() {
                    if i > 0 {
                        label.push_str(", ");
                    }
                    let (a, b) = (p.first()?.start as usize, p.last()?.end() as usize);
                    let text = head[a..b].split_whitespace().collect::<Vec<_>>().join(" ");
                    let from = label.encode_utf16().count() as u32;
                    label.push_str(&text);
                    params.push((from, from + text.encode_utf16().count() as u32));
                    variadic |= p.iter().any(|t| t.kind == K::Ellipsis);
                    let mut ty = &p[..top_level(p, K::Eq).unwrap_or(p.len())];
                    let named = |t: &crate::lexer::Token| matches!(t.kind, K::Ident | K::Gt | K::Star | K::Amp | K::AmpAmp);
                    if ty.len() >= 2 && ty[ty.len() - 1].kind == K::Ident && named(&ty[ty.len() - 2]) {
                        ty = &ty[..ty.len() - 1];
                    }
                    tys.push(declared_type_tail(ty, head));
                }
            }
            label.push(')');
            Some(Overload { id: c, info: SigInfo { label, params }, tys, variadic })
        })
    }
}
