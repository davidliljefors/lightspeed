//! `using namespace` directives, using-declarations and namespace aliases.
//!
//! They make names visible without a qualifier (or give a qualifier another
//! spelling), so lookups need them from the file itself and from everything it
//! includes. [`scan`] extracts them from a token stream in one linear pass;
//! the index stores each file's namespace-level ones and unions them over the
//! include closure on demand.

use crate::lexer::{Keyword, Token, TokenKind};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScopeDecls {
    /// Every identifier named by a using-directive or using-declaration:
    /// `using namespace a::b;` contributes `a` and `b`.
    pub names: Vec<String>,
    /// `namespace alias = a::b;` as `("alias", "a::b")`.
    pub aliases: Vec<(String, String)>,
}

impl ScopeDecls {
    pub fn heap_bytes(&self) -> usize {
        self.names.iter().map(|s| s.capacity() + 24).sum::<usize>() + self.aliases.iter().map(|(a, b)| a.capacity() + b.capacity() + 48).sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.aliases.is_empty()
    }

    pub fn names_contain(&self, name: &str) -> bool {
        self.names.iter().any(|n| n == name)
    }

    /// What the namespace alias `alias` stands for.
    pub fn alias(&self, alias: &str) -> Option<&str> {
        self.aliases.iter().find(|(a, _)| a == alias).map(|(_, t)| t.as_str())
    }

    pub fn merge(&mut self, other: &ScopeDecls) {
        self.names.extend_from_slice(&other.names);
        self.aliases.extend_from_slice(&other.aliases);
    }

    pub fn normalize(&mut self) {
        self.names.sort_unstable();
        self.names.dedup();
        self.aliases.sort_unstable();
        self.aliases.dedup();
    }
}

/// Whether the `{` at `toks[i]` opens a namespace (or `extern "C"`) block.
fn opens_namespace(toks: &[Token], i: usize) -> bool {
    let mut k = i;
    while k > 0 && matches!(toks[k - 1].kind, TokenKind::Ident | TokenKind::ColonColon) {
        k -= 1;
    }
    if k > 0 && toks[k - 1].is_keyword(Keyword::Namespace) {
        return true;
    }
    i >= 2 && toks[i - 1].kind == TokenKind::String && toks[i - 2].is_keyword(Keyword::Extern)
}

/// Declarations among `toks[..at]`.
///
/// With `exported` only those outside any class or function body are kept and
/// all of them stay valid to the end (what an includer sees). Otherwise every
/// one still in scope at `at` is kept: declared earlier, in a block not yet
/// closed.
pub fn scan(toks: &[Token], text: &str, at: usize, exported: bool) -> ScopeDecls {
    let mut names: Vec<(usize, &str)> = Vec::new();
    let mut aliases: Vec<(usize, &str, String)> = Vec::new();
    let mut blocks: Vec<bool> = Vec::new();
    let mut ns_only = true;
    let end = at.min(toks.len());
    let mut i = 0;
    while i < end {
        let t = &toks[i];
        match t.kind {
            TokenKind::LBrace => {
                blocks.push(opens_namespace(toks, i));
                ns_only &= blocks[blocks.len() - 1];
            }
            TokenKind::RBrace => {
                blocks.pop();
                ns_only = blocks.iter().all(|&b| b);
                if !exported {
                    names.retain(|&(d, _)| d <= blocks.len());
                    aliases.retain(|(d, ..)| *d <= blocks.len());
                }
            }
            _ if t.is_keyword(Keyword::Using) => {
                let mut j = i + 1;
                let mut idents: Vec<&str> = Vec::new();
                let mut closed = false;
                while let Some(u) = toks.get(j) {
                    match u.kind {
                        TokenKind::Semi => {
                            closed = true;
                            break;
                        }
                        TokenKind::Eq | TokenKind::LBrace | TokenKind::RBrace => break,
                        TokenKind::Ident => idents.push(u.text(text)),
                        _ => {}
                    }
                    j += 1;
                }
                if closed && (!exported || ns_only) {
                    names.extend(idents.into_iter().map(|n| (blocks.len(), n)));
                }
                i = j;
            }
            _ if t.is_keyword(Keyword::Namespace)
                && toks.get(i + 1).is_some_and(|n| n.kind == TokenKind::Ident)
                && toks.get(i + 2).is_some_and(|n| n.kind == TokenKind::Eq) =>
            {
                let mut target = String::new();
                let mut j = i + 3;
                while let Some(u) = toks.get(j) {
                    match u.kind {
                        TokenKind::Semi => break,
                        TokenKind::Ident => {
                            if !target.is_empty() {
                                target.push_str("::");
                            }
                            target.push_str(u.text(text));
                        }
                        TokenKind::LBrace | TokenKind::RBrace => {
                            target.clear();
                            break;
                        }
                        _ => {}
                    }
                    j += 1;
                }
                if !target.is_empty() && (!exported || ns_only) {
                    aliases.push((blocks.len(), toks[i + 1].text(text), target));
                }
                i = j;
            }
            _ => {}
        }
        i += 1;
    }
    ScopeDecls {
        names: names.into_iter().map(|(_, n)| n.to_owned()).collect(),
        aliases: aliases.into_iter().map(|(_, a, t)| (a.to_owned(), t)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex_into;

    fn run(src: &str, at: Option<&str>, exported: bool) -> ScopeDecls {
        let mut toks = Vec::new();
        lex_into(src, &mut toks);
        let at = at.map_or(toks.len(), |m| toks.partition_point(|t| (t.start as usize) < src.find(m).unwrap()));
        let mut d = scan(&toks, src, at, exported);
        d.normalize();
        d
    }

    #[test]
    fn directives_declarations_and_aliases() {
        let src = "using namespace a::b;\nusing c::d;\nusing Id = int;\nnamespace fs = std::filesystem;\nnamespace n { using namespace inner; }\n\
                   struct S { using Base::f; };\nvoid fn() { using namespace local; namespace x = y; }\nint after;\n";
        let d = run(src, None, true);
        assert_eq!(d.names, ["a", "b", "c", "d", "inner"]);
        assert_eq!(d.aliases, [("fs".to_owned(), "std::filesystem".to_owned())]);

        // Position-aware: function-local ones only inside the function, never after it.
        let d = run(src, Some("int after"), false);
        assert!(!d.names_contain("local") && d.names_contain("a") && !d.names_contain("inner"));
        assert_eq!(d.alias("fs"), Some("std::filesystem"));
        assert_eq!(d.alias("x"), None);
        let d = run(src, Some("namespace x"), false);
        assert!(d.names_contain("local"));
    }
}
