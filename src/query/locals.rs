//! Parameters and local variables. They are not indexed symbols; instead the
//! enclosing function is re-scanned on demand and names are bound lexically
//! (innermost visible declaration wins), so hover, definition, references and
//! rename on them never leak into same-named symbols elsewhere.

use super::types::matching_fwd;
use super::{Query, ident_at, sig_tokens};
use crate::lexer::{Keyword, Token, TokenKind as K};
use crate::symbols::SymbolId;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LocalKind {
    Param,
    Variable,
    TemplateParam,
}

pub struct Local {
    pub kind: LocalKind,
    pub name: String,
    /// Declaration text on one line, e.g. `const std::string& name`.
    pub signature: String,
    /// The declared type is `auto`, so the type has to be inferred.
    pub auto: bool,
    /// The function the name is local to.
    pub function: SymbolId,
    /// Byte range of the declaring identifier.
    pub decl: (u32, u32),
    /// Every occurrence bound to this declaration (declaration included), in source order.
    pub uses: Vec<(u32, u32)>,
    /// Byte range of the identifier that was asked about.
    pub hovered: (u32, u32),
    /// Position of the declaration's type start, used to infer `auto`.
    pub decl_offset: u32,
}

struct Declared {
    name: usize,
    ty_start: usize,
    /// End of the type name (before `*`/`&`), and where this declarator begins.
    ty_end: usize,
    decl_start: usize,
    end: usize,
    param: bool,
    /// Element of a structured binding `auto& [a, b]`.
    binding: bool,
}

const NON_TYPE: &[&str] = &[
    "return", "delete", "new", "case", "else", "do", "goto", "throw", "co_return", "co_yield", "co_await", "sizeof", "using", "namespace", "typedef",
    "default", "break", "continue", "not", "and", "or", "if", "for", "while", "switch", "try", "catch", "this",
];
const BUILTIN: &[&str] = &["unsigned", "signed", "long", "short", "int", "char", "double", "float", "bool", "void", "wchar_t"];
const HEADER_KW: &[&str] = &["for", "if", "while", "switch", "catch"];

fn is_specifier(t: &Token, text: &str) -> bool {
    use Keyword as W;
    t.kind == K::Keyword
        && matches!(
            t.keyword,
            W::Const | W::Constexpr | W::Consteval | W::Constinit | W::Static | W::Inline | W::Volatile | W::Mutable | W::Typename | W::Struct | W::Class | W::Union | W::Enum | W::Extern
        )
        || t.kind == K::Ident && matches!(t.text(text), "thread_local" | "register")
}

/// Declarators of a `type name [= init], name2 ...` that starts at `toks[s]`.
/// `limit` bounds the tokens looked at; `close_ok` allows `)` to follow a name
/// (parameter lists, `catch`).
fn parse_decl(toks: &[Token], text: &str, s: usize, limit: usize, param: bool, close_ok: bool, out: &mut Vec<Declared>) -> Option<()> {
    let at = |i: usize| toks[..limit].get(i);
    let ty_start = s;
    let mut i = s;
    while at(i).is_some_and(|t| is_specifier(t, text)) {
        i += 1;
    }
    if at(i)?.kind == K::ColonColon {
        i += 1;
    }
    let first = at(i).filter(|t| t.kind == K::Ident)?.text(text);
    if NON_TYPE.contains(&first) {
        return None;
    }
    i += 1;
    if BUILTIN.contains(&first) {
        while at(i).is_some_and(|t| t.kind == K::Ident && BUILTIN.contains(&t.text(text))) {
            i += 1;
        }
    } else {
        loop {
            if at(i).is_some_and(|t| t.kind == K::Lt) {
                let c = matching_fwd(&toks[..limit], i)?;
                if toks[i..c].iter().any(|t| matches!(t.kind, K::Semi | K::LBrace | K::EqEq | K::NotEq | K::Le | K::Question)) {
                    return None;
                }
                i = c + 1;
            }
            if at(i).is_some_and(|t| t.kind == K::ColonColon) && at(i + 1).is_some_and(|t| t.kind == K::Ident) {
                i += 2;
            } else {
                break;
            }
        }
    }
    let ty_end = i;
    if first == "auto" {
        let mut j = i;
        while at(j).is_some_and(|t| matches!(t.kind, K::Star | K::Amp | K::AmpAmp) || t.is_keyword(Keyword::Const)) {
            j += 1;
        }
        if at(j).is_some_and(|t| t.kind == K::LBracket)
            && let Some(c) = matching_fwd(&toks[..limit], j)
        {
            for (n, t) in toks.iter().enumerate().take(c).skip(j + 1) {
                if t.kind == K::Ident {
                    out.push(Declared { name: n, ty_start, ty_end, decl_start: ty_end, end: c + 1, param, binding: true });
                }
            }
            return Some(());
        }
    }
    let mut first_declarator = true;
    loop {
        let decl_start = i;
        while at(i).is_some_and(|t| matches!(t.kind, K::Star | K::Amp | K::AmpAmp) || t.is_keyword(Keyword::Const) || t.is_keyword(Keyword::Volatile)) {
            i += 1;
        }
        let name = at(i).filter(|t| t.kind == K::Ident)?;
        if first_declarator {
            let ok = match at(i + 1).map(|t| t.kind) {
                Some(K::Semi | K::Eq | K::LBrace | K::LParen | K::Comma | K::LBracket | K::Colon) => true,
                Some(K::RParen) => close_ok,
                None => param,
                _ => false,
            };
            if !ok || NON_TYPE.contains(&name.text(text)) {
                return None;
            }
            first_declarator = false;
        }
        let mut end = i + 1;
        if at(end).is_some_and(|t| t.kind == K::LBracket)
            && let Some(c) = matching_fwd(&toks[..limit], end)
        {
            end = c + 1;
        }
        // Walk the initializer to the next top-level comma.
        let mut j = i + 1;
        let mut default_at = None;
        while let Some(t) = at(j) {
            match t.kind {
                K::LParen | K::LBracket | K::LBrace => match matching_fwd(&toks[..limit], j) {
                    Some(c) => j = c,
                    None => break,
                },
                K::Eq if default_at.is_none() => default_at = Some(j),
                K::Comma | K::Semi | K::RParen | K::Colon => break,
                _ => {}
            }
            j += 1;
        }
        if param {
            end = default_at.unwrap_or(j.min(limit));
        }
        out.push(Declared { name: i, ty_start, ty_end, decl_start, end, param, binding: false });
        if param || at(j).is_none_or(|t| t.kind != K::Comma) {
            return Some(());
        }
        i = j + 1;
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Group {
    Brace,
    Paren,
    Lambda,
    Bracket,
}

fn squash(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for w in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

impl Query<'_> {
    /// The parameter or local variable the identifier at `offset` is bound to,
    /// with all its occurrences, or `None` when it names something else.
    pub fn local_at(&self, path: &str, text: &str, offset: u32) -> Option<Local> {
        let toks = sig_tokens(text);
        let hit = ident_at(&toks, offset)?;
        self.local_in(&toks, path, text, hit)
    }

    /// Like [`Self::local_at`] for the identifier token `toks[hit]` of an already lexed file.
    pub(super) fn local_in(&self, toks: &[Token], path: &str, text: &str, hit: usize) -> Option<Local> {
        self.function_local_in(toks, path, text, hit).or_else(|| self.template_param_in(toks, path, text, hit))
    }

    fn function_local_in(&self, toks: &[Token], path: &str, text: &str, hit: usize) -> Option<Local> {
        let name = toks[hit].text(text);
        let offset = toks[hit].start;
        if hit > 0 && matches!(toks[hit - 1].kind, K::Dot | K::Arrow | K::ColonColon) {
            return None;
        }
        let (db, fi) = (&self.idx.db, self.idx.file_index(path)?);
        let mut best: Option<(u32, SymbolId)> = None;
        for id in self.idx.file_symbols(fi) {
            let s = db.symbol(id);
            if s.kind.is_callable() && s.extent_start <= offset && offset <= s.extent_end {
                let len = s.extent_end - s.extent_start;
                if best.is_none_or(|(l, _)| len <= l) {
                    best = Some((len, id));
                }
            }
        }
        let function = best?.1;
        let name_tok = toks.binary_search_by_key(&db.symbol(function).name_offset, |t| t.start).ok()?;

        // Parameter list.
        let lparen = if toks[name_tok].is_keyword(Keyword::Operator) && toks.get(name_tok + 1).is_some_and(|t| t.kind == K::LParen) && toks.get(name_tok + 2).is_some_and(|t| t.kind == K::RParen) {
            name_tok + 3
        } else {
            (name_tok..toks.len()).find(|&i| toks[i].kind == K::LParen)?
        };
        let rparen = matching_fwd(toks, lparen)?;

        // Body: after an optional constructor-initializer list.
        let mut body = None;
        let (mut j, mut init) = (rparen + 1, false);
        while let Some(t) = toks.get(j) {
            match t.kind {
                K::Semi => break,
                K::Colon => init = true,
                K::LParen => j = matching_fwd(toks, j).unwrap_or(j),
                K::LBrace if init && j > 0 && matches!(toks[j - 1].kind, K::Ident | K::Gt) => j = matching_fwd(toks, j).unwrap_or(j),
                K::LBrace => {
                    body = matching_fwd(toks, j).map(|c| (j, c));
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        let (body_open, end) = body.unwrap_or((rparen + 1, rparen));
        if hit < lparen || hit > end {
            return None;
        }

        let mut decls: Vec<Declared> = Vec::new();
        let mut scratch: Vec<Declared> = Vec::new();
        let mut occ: Vec<(usize, Option<usize>)> = Vec::new();
        let mut scopes: Vec<Vec<usize>> = vec![Vec::new()];
        let mut pending: Vec<(usize, usize)> = Vec::new();
        let mut groups: Vec<Group> = Vec::new();

        let register = |scratch: &mut Vec<Declared>,
                        decls: &mut Vec<Declared>,
                        occ: &mut Vec<(usize, Option<usize>)>,
                        scopes: &mut Vec<Vec<usize>>,
                        pending: &mut Vec<(usize, usize)>,
                        depth: usize,
                        in_header: bool| {
            for d in scratch.drain(..) {
                if toks[d.name].text(text) != name {
                    continue;
                }
                let di = decls.len();
                occ.push((d.name, Some(di)));
                if in_header {
                    pending.push((depth, di));
                } else {
                    scopes.last_mut().unwrap().push(di);
                }
                decls.push(d);
            }
        };

        // Parameters, split at top-level commas.
        let mut seg = lparen + 1;
        let mut k = seg;
        while k <= rparen {
            if k == rparen || toks[k].kind == K::Comma {
                if k > seg {
                    parse_decl(toks, text, seg, k, true, true, &mut scratch);
                    register(&mut scratch, &mut decls, &mut occ, &mut scopes, &mut pending, 0, false);
                }
                seg = k + 1;
            } else if matches!(toks[k].kind, K::LParen | K::LBracket | K::LBrace | K::Lt)
                && let Some(c) = matching_fwd(&toks[..rparen], k)
            {
                k = c;
            }
            k += 1;
        }

        let lookup = |pending: &[(usize, usize)], scopes: &[Vec<usize>]| -> Option<usize> {
            pending.last().map(|&(_, d)| d).or_else(|| scopes.iter().rev().find_map(|s| s.last().copied()))
        };

        let mut i = rparen + 1;
        while body.is_some() && i <= end {
            let t = &toks[i];
            // Statement starts.
            let prev = i.checked_sub(1).map(|p| toks[p].kind);
            let top = groups.last().copied();
            let (starts, close_ok) = match prev {
                Some(K::Semi | K::LBrace | K::RBrace | K::RParen) if i > body_open => (true, false),
                Some(K::Colon) if i > body_open && !matches!(top, Some(Group::Paren | Group::Lambda)) => (true, false),
                Some(K::Ident) if i > body_open && matches!(toks[i - 1].text(text), "else" | "do") => (true, false),
                Some(K::LParen) if i > body_open => {
                    let before = toks.get(i.wrapping_sub(2)).filter(|_| i >= 2);
                    let header = before.is_some_and(|b| b.kind == K::Ident && HEADER_KW.contains(&b.text(text)));
                    let lambda = before.is_some_and(|b| b.kind == K::RBracket);
                    (header || lambda, lambda || before.is_some_and(|b| b.text(text) == "catch"))
                }
                Some(K::Comma) if top == Some(Group::Lambda) => (true, true),
                _ => (false, false),
            };
            if starts && t.kind != K::RBrace {
                parse_decl(toks, text, i, end + 1, false, close_ok, &mut scratch);
                let in_header = matches!(top, Some(Group::Paren | Group::Lambda));
                register(&mut scratch, &mut decls, &mut occ, &mut scopes, &mut pending, groups.len(), in_header);
            }

            match t.kind {
                K::LBrace => {
                    pending_into_scope(&mut pending, &mut scopes, groups.len());
                    groups.push(Group::Brace);
                }
                K::RBrace => {
                    groups.pop();
                    if scopes.len() > 1 {
                        scopes.pop();
                    }
                }
                K::LParen => {
                    let lambda = i > 0 && toks[i - 1].kind == K::RBracket;
                    groups.push(if lambda { Group::Lambda } else { Group::Paren });
                }
                K::LBracket => groups.push(Group::Bracket),
                K::RParen | K::RBracket => {
                    groups.pop();
                }
                K::Semi => {
                    let d = groups.len() + 1;
                    pending.retain(|&(pd, _)| pd != d);
                }
                K::Ident if t.text(text) == name => {
                    let skip = i > 0 && matches!(toks[i - 1].kind, K::Dot | K::Arrow | K::ColonColon);
                    let member_init = i < body_open && matches!(toks[i - 1].kind, K::Colon | K::Comma) && toks.get(i + 1).is_some_and(|n| matches!(n.kind, K::LParen | K::LBrace));
                    if !skip && !member_init && !decls.iter().any(|d| d.name == i) {
                        occ.push((i, lookup(&pending, &scopes)));
                    }
                }
                _ => {}
            }
            i += 1;
        }

        let hit_binding = occ.iter().find(|(tok, _)| *tok == hit)?.1?;
        let d = &decls[hit_binding];
        let uses: Vec<(u32, u32)> = occ.iter().filter(|(_, b)| *b == Some(hit_binding)).map(|&(tok, _)| (toks[tok].start, toks[tok].end())).collect();
        let span = |a: usize, b: usize| &text[toks[a].start as usize..toks[b - 1].end() as usize];
        let signature = if d.decl_start == d.ty_end {
            squash(span(d.ty_start, d.end))
        } else {
            format!("{} {}", squash(span(d.ty_start, d.ty_end)), squash(span(d.decl_start, d.end)))
        };
        Some(Local {
            kind: if d.param { LocalKind::Param } else { LocalKind::Variable },
            name: name.to_owned(),
            signature,
            auto: toks[d.ty_start..d.ty_end].iter().any(|t| t.text(text) == "auto"),
            function,
            decl: (toks[d.name].start, toks[d.name].end()),
            uses,
            hovered: (toks[hit].start, toks[hit].end()),
            decl_offset: if d.binding { toks[d.ty_start].start } else { toks[d.name].start },
        })
    }
}

/// Moves declarations waiting on a `for`/`if`/lambda header into the block
/// that header opens: `depth` is the group depth just outside the new `{`.
fn pending_into_scope(pending: &mut Vec<(usize, usize)>, scopes: &mut Vec<Vec<usize>>, depth: usize) {
    let mut fresh = Vec::new();
    pending.retain(|&(pd, d)| {
        if pd == depth + 1 {
            fresh.push(d);
            false
        } else {
            true
        }
    });
    scopes.push(fresh);
}
