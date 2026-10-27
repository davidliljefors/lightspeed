//! Zero-allocation, error-tolerant C++ tokenizer.
//!
//! Tokens are 12-byte `Copy` records holding offsets into the source; text is
//! recovered with [`Token::text`]. The lexer never fails: unknown bytes become
//! [`TokenKind::Unknown`] and unterminated literals/comments carry
//! [`TokenFlags::UNTERMINATED`].
//!
//! Deliberate simplifications (speed over standards):
//! * `>` is always a single [`TokenKind::Gt`] so `vector<vector<int>>` needs no
//!   token splitting; the preprocessor merges adjacent `>`/`>=` as needed.
//! * Digraphs/trigraphs, `u8`-suffixed user-defined literals and `#include`
//!   header-names are not special-cased; header names are rebuilt from the
//!   source span between `<` and `>` by the preprocessor.
//! * Line splices (`\` newline) are treated as whitespace, including inside
//!   `//` comments, but not inside identifiers or other tokens.
//! * Any byte >= 0x80 is an identifier character (UTF-8 identifiers).
//! * A newline-free run of comments/whitespace keeps `BOL` alive, so
//!   `/* x */ #define A` still reads as a directive.

use memchr::{memchr, memchr3};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TokenKind {
    Ident,
    /// Reserved word recognised by the declaration scanner; see [`Keyword`].
    Keyword,
    /// pp-number (`42`, `0x1F`, `1'000`, `1.5e-3f`).
    Number,
    /// String literal incl. prefix and raw strings.
    String,
    /// Character literal incl. prefix.
    Char,
    /// Only emitted when [`Lexer::emit_comments`] is on.
    Comment,
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Semi,
    Comma,
    Colon,
    ColonColon,
    Dot,
    Ellipsis,
    Arrow,
    Hash,
    HashHash,
    Lt,
    /// Always a single `>`.
    Gt,
    Star,
    Amp,
    AmpAmp,
    Tilde,
    Eq,
    EqEq,
    NotEq,
    /// `<=`
    Le,
    /// `<<`
    Shl,
    Question,
    Plus,
    Minus,
    Slash,
    Percent,
    Pipe,
    PipePipe,
    Caret,
    Bang,
    /// Other compound operators: `+= -= *= /= %= &= |= ^= <<= ++ -- ->* .* <=>`.
    Op,
    /// Stray byte (`@`, backtick, lone `\`, control chars).
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Keyword {
    None,
    Class,
    Struct,
    Union,
    Enum,
    Namespace,
    Template,
    Typename,
    Using,
    Typedef,
    Public,
    Private,
    Protected,
    Virtual,
    /// Contextual in C++; callers must check position.
    Override,
    /// Contextual in C++; callers must check position.
    Final,
    Static,
    Const,
    Constexpr,
    Consteval,
    Constinit,
    Inline,
    Explicit,
    Operator,
    Friend,
    Extern,
    Noexcept,
    Mutable,
    Volatile,
    Requires,
    Concept,
    Delete,
    Default,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct TokenFlags(u8);

impl TokenFlags {
    /// First token on its (logical) line; marks `#` directives.
    pub const BOL: Self = Self(1);
    /// Whitespace/comment/newline precedes the token.
    pub const SPACE_BEFORE: Self = Self(2);
    /// Literal or block comment hit end-of-line/file without closing.
    pub const UNTERMINATED: Self = Self(4);

    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[inline]
    fn set(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Token {
    pub start: u32,
    pub len: u32,
    pub kind: TokenKind,
    pub keyword: Keyword,
    pub flags: TokenFlags,
}

const _: () = assert!(size_of::<Token>() == 12);

impl Token {
    #[inline]
    pub fn end(&self) -> u32 {
        self.start + self.len
    }

    /// Token text. `src` must be the text the token was lexed from.
    #[inline]
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start as usize..self.end() as usize]
    }

    #[inline]
    pub fn is(&self, kind: TokenKind) -> bool {
        self.kind == kind
    }

    #[inline]
    pub fn is_keyword(&self, kw: Keyword) -> bool {
        self.keyword == kw
    }

    #[inline]
    pub fn at_line_start(&self) -> bool {
        self.flags.contains(TokenFlags::BOL)
    }

    #[inline]
    pub fn space_before(&self) -> bool {
        self.flags.contains(TokenFlags::SPACE_BEFORE)
    }

    #[inline]
    pub fn unterminated(&self) -> bool {
        self.flags.contains(TokenFlags::UNTERMINATED)
    }
}

const IDENT_START: u8 = 1;
const IDENT_CONT: u8 = 2;
const DIGIT: u8 = 4;

static CLASS: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let c = i as u8;
        if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80 {
            t[i] = IDENT_START | IDENT_CONT;
        } else if c.is_ascii_digit() {
            t[i] = DIGIT | IDENT_CONT;
        }
        i += 1;
    }
    t
};

fn keyword(s: &[u8]) -> Keyword {
    if s.len() < 3 || s.len() > 9 || !s[0].is_ascii_lowercase() {
        return Keyword::None;
    }
    match s {
        b"class" => Keyword::Class,
        b"struct" => Keyword::Struct,
        b"union" => Keyword::Union,
        b"enum" => Keyword::Enum,
        b"namespace" => Keyword::Namespace,
        b"template" => Keyword::Template,
        b"typename" => Keyword::Typename,
        b"using" => Keyword::Using,
        b"typedef" => Keyword::Typedef,
        b"public" => Keyword::Public,
        b"private" => Keyword::Private,
        b"protected" => Keyword::Protected,
        b"virtual" => Keyword::Virtual,
        b"override" => Keyword::Override,
        b"final" => Keyword::Final,
        b"static" => Keyword::Static,
        b"const" => Keyword::Const,
        b"constexpr" => Keyword::Constexpr,
        b"consteval" => Keyword::Consteval,
        b"constinit" => Keyword::Constinit,
        b"inline" => Keyword::Inline,
        b"explicit" => Keyword::Explicit,
        b"operator" => Keyword::Operator,
        b"friend" => Keyword::Friend,
        b"extern" => Keyword::Extern,
        b"noexcept" => Keyword::Noexcept,
        b"mutable" => Keyword::Mutable,
        b"volatile" => Keyword::Volatile,
        b"requires" => Keyword::Requires,
        b"concept" => Keyword::Concept,
        b"delete" => Keyword::Delete,
        b"default" => Keyword::Default,
        _ => Keyword::None,
    }
}

fn string_prefix(prefix: &[u8], quote: u8) -> bool {
    match prefix {
        b"L" | b"u" | b"U" | b"u8" => true,
        b"R" | b"LR" | b"uR" | b"UR" | b"u8R" => quote == b'"',
        _ => false,
    }
}

/// Length of a `\`-newline splice at `p`, or 0.
#[inline]
fn splice_len(b: &[u8], p: usize) -> usize {
    match b.get(p + 1) {
        Some(b'\n') => 2,
        Some(b'\r') if b.get(p + 2) == Some(&b'\n') => 3,
        _ => 0,
    }
}

/// Position of the `\n` ending the `//` comment at `p` (or EOF), honouring splices.
fn line_comment_end(b: &[u8], p: usize) -> usize {
    let mut i = p + 2;
    loop {
        match memchr(b'\n', &b[i..]) {
            None => return b.len(),
            Some(o) => {
                let nl = i + o;
                let spliced = (nl > p + 2 && b[nl - 1] == b'\\')
                    || (nl > p + 3 && b[nl - 1] == b'\r' && b[nl - 2] == b'\\');
                if !spliced {
                    return nl;
                }
                i = nl + 1;
            }
        }
    }
}

/// `(end, terminated)` for the `/*` comment at `p`.
fn block_comment_end(b: &[u8], p: usize) -> (usize, bool) {
    let mut i = p + 2;
    while let Some(o) = memchr(b'*', &b[i..]) {
        let j = i + o;
        if b.get(j + 1) == Some(&b'/') {
            return (j + 2, true);
        }
        i = j + 1;
    }
    (b.len(), false)
}

#[inline]
fn scan_ident(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && CLASS[b[i] as usize] & IDENT_CONT != 0 {
        i += 1;
    }
    i
}

/// `(end, terminated)` for the quoted literal whose opening quote is at `q`.
fn scan_quoted(b: &[u8], q: usize) -> (usize, bool) {
    let quote = b[q];
    let mut i = q + 1;
    loop {
        if i >= b.len() {
            return (b.len(), false);
        }
        match memchr3(quote, b'\\', b'\n', &b[i..]) {
            None => return (b.len(), false),
            Some(o) => {
                let j = i + o;
                match b[j] {
                    b'\n' => return (j, false),
                    b'\\' => {
                        i = if b.get(j + 1) == Some(&b'\r') && b.get(j + 2) == Some(&b'\n') {
                            j + 3
                        } else {
                            j + 2
                        };
                    }
                    _ => return (j + 1, true),
                }
            }
        }
    }
}

/// `(end, terminated)` for a raw string whose `"` is at `q`.
fn scan_raw_string(b: &[u8], q: usize) -> (usize, bool) {
    let d0 = q + 1;
    let mut k = d0;
    while k < b.len() && k - d0 <= 16 && !matches!(b[k], b'(' | b')' | b'"' | b'\\' | b' ' | b'\n') {
        k += 1;
    }
    if k >= b.len() || b[k] != b'(' || k - d0 > 16 {
        return scan_quoted(b, q); // malformed delimiter: degrade to a plain string
    }
    let delim = &b[d0..k];
    let mut i = k + 1;
    while let Some(o) = memchr(b')', &b[i..]) {
        let j = i + o;
        let after = j + 1 + delim.len();
        if b.get(j + 1..after) == Some(delim) && b.get(after) == Some(&b'"') {
            return (after + 1, true);
        }
        i = j + 1;
    }
    (b.len(), false)
}

fn scan_number(b: &[u8], mut i: usize) -> usize {
    while i < b.len() {
        let c = b[i];
        if matches!(c, b'e' | b'E' | b'p' | b'P') && matches!(b.get(i + 1), Some(b'+' | b'-')) {
            i += 2;
        } else if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' {
            i += 1;
        } else if c == b'\'' && b.get(i + 1).is_some_and(|n| n.is_ascii_alphanumeric()) {
            i += 2; // digit separator
        } else {
            break;
        }
    }
    i
}

fn punct(c: u8, n1: u8, n2: u8) -> (TokenKind, usize) {
    use TokenKind::*;
    match c {
        b'{' => (LBrace, 1),
        b'}' => (RBrace, 1),
        b'(' => (LParen, 1),
        b')' => (RParen, 1),
        b'[' => (LBracket, 1),
        b']' => (RBracket, 1),
        b';' => (Semi, 1),
        b',' => (Comma, 1),
        b'~' => (Tilde, 1),
        b'?' => (Question, 1),
        b'>' => (Gt, 1),
        b':' if n1 == b':' => (ColonColon, 2),
        b':' => (Colon, 1),
        b'.' if n1 == b'.' && n2 == b'.' => (Ellipsis, 3),
        b'.' if n1 == b'*' => (Op, 2),
        b'.' => (Dot, 1),
        b'-' => match (n1, n2) {
            (b'>', b'*') => (Op, 3),
            (b'>', _) => (Arrow, 2),
            (b'-' | b'=', _) => (Op, 2),
            _ => (Minus, 1),
        },
        b'+' if n1 == b'+' || n1 == b'=' => (Op, 2),
        b'+' => (Plus, 1),
        b'*' if n1 == b'=' => (Op, 2),
        b'*' => (Star, 1),
        b'/' if n1 == b'=' => (Op, 2),
        b'/' => (Slash, 1),
        b'%' if n1 == b'=' => (Op, 2),
        b'%' => (Percent, 1),
        b'&' => match n1 {
            b'&' => (AmpAmp, 2),
            b'=' => (Op, 2),
            _ => (Amp, 1),
        },
        b'|' => match n1 {
            b'|' => (PipePipe, 2),
            b'=' => (Op, 2),
            _ => (Pipe, 1),
        },
        b'^' if n1 == b'=' => (Op, 2),
        b'^' => (Caret, 1),
        b'!' if n1 == b'=' => (NotEq, 2),
        b'!' => (Bang, 1),
        b'=' if n1 == b'=' => (EqEq, 2),
        b'=' => (Eq, 1),
        b'<' => match (n1, n2) {
            (b'<', b'=') => (Op, 3),
            (b'<', _) => (Shl, 2),
            (b'=', b'>') => (Op, 3),
            (b'=', _) => (Le, 2),
            _ => (Lt, 1),
        },
        b'#' if n1 == b'#' => (HashHash, 2),
        b'#' => (Hash, 1),
        _ => (Unknown, 1),
    }
}

/// Streaming tokenizer over a `&str`. Implements `Iterator<Item = Token>`.
#[derive(Clone)]
pub struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    at_bol: bool,
    emit_comments: bool,
}

impl<'a> Lexer<'a> {
    /// Panics if `src` is 4 GiB or larger (token offsets are `u32`).
    pub fn new(src: &'a str) -> Self {
        assert!(src.len() as u64 <= u32::MAX as u64, "source exceeds 4 GiB");
        Self { src, pos: 0, at_bol: true, emit_comments: false }
    }

    /// Emit `//` and `/* */` comments as [`TokenKind::Comment`] tokens.
    pub fn emit_comments(mut self, yes: bool) -> Self {
        self.emit_comments = yes;
        self
    }

    pub fn source(&self) -> &'a str {
        self.src
    }

    pub fn next_token(&mut self) -> Option<Token> {
        let b = self.src.as_bytes();
        let len = b.len();
        let mut p = self.pos;
        let mut bol = self.at_bol;
        let mut flags = TokenFlags::default();

        loop {
            if p >= len {
                self.pos = p;
                self.at_bol = bol;
                return None;
            }
            match b[p] {
                b'\n' => {
                    p += 1;
                    bol = true;
                    flags.set(TokenFlags::SPACE_BEFORE);
                }
                b' ' | b'\t' | b'\r' | 0x0B | 0x0C => {
                    p += 1;
                    flags.set(TokenFlags::SPACE_BEFORE);
                }
                b'\\' if splice_len(b, p) > 0 => {
                    p += splice_len(b, p);
                    flags.set(TokenFlags::SPACE_BEFORE);
                }
                b'/' if b.get(p + 1) == Some(&b'/') => {
                    let end = line_comment_end(b, p);
                    if self.emit_comments {
                        return Some(self.comment(p, end, true, bol, flags));
                    }
                    p = end;
                    flags.set(TokenFlags::SPACE_BEFORE);
                }
                b'/' if b.get(p + 1) == Some(&b'*') => {
                    let (end, closed) = block_comment_end(b, p);
                    // A multi-line comment is one space (translation phase 3), so it
                    // neither starts nor ends a logical line.
                    if self.emit_comments {
                        return Some(self.comment(p, end, closed, bol, flags));
                    }
                    p = end;
                    flags.set(TokenFlags::SPACE_BEFORE);
                }
                _ => break,
            }
        }

        let start = p;
        let c = b[p];
        let mut kw = Keyword::None;
        let mut terminated = true;
        let (kind, end) = if CLASS[c as usize] & IDENT_START != 0 {
            let id_end = scan_ident(b, p + 1);
            match b.get(id_end) {
                Some(&q @ (b'"' | b'\'')) if string_prefix(&b[p..id_end], q) => {
                    let (end, t) = if q == b'"' && b[id_end - 1] == b'R' {
                        scan_raw_string(b, id_end)
                    } else {
                        scan_quoted(b, id_end)
                    };
                    terminated = t;
                    (if q == b'"' { TokenKind::String } else { TokenKind::Char }, end)
                }
                _ => {
                    kw = keyword(&b[p..id_end]);
                    (if kw == Keyword::None { TokenKind::Ident } else { TokenKind::Keyword }, id_end)
                }
            }
        } else if CLASS[c as usize] & DIGIT != 0 || (c == b'.' && b.get(p + 1).is_some_and(u8::is_ascii_digit)) {
            (TokenKind::Number, scan_number(b, p + 1))
        } else if c == b'"' || c == b'\'' {
            let (end, t) = scan_quoted(b, p);
            terminated = t;
            (if c == b'"' { TokenKind::String } else { TokenKind::Char }, end)
        } else {
            let n1 = b.get(p + 1).copied().unwrap_or(0);
            let n2 = b.get(p + 2).copied().unwrap_or(0);
            let (k, n) = punct(c, n1, n2);
            (k, p + n)
        };

        if bol {
            flags.set(TokenFlags::BOL);
        }
        if !terminated {
            flags.set(TokenFlags::UNTERMINATED);
        }
        self.pos = end;
        self.at_bol = false;
        Some(Token { start: start as u32, len: (end - start) as u32, kind, keyword: kw, flags })
    }

    fn comment(
        &mut self,
        start: usize,
        end: usize,
        closed: bool,
        bol: bool,
        mut flags: TokenFlags,
    ) -> Token {
        if bol {
            flags.set(TokenFlags::BOL);
        }
        if !closed {
            flags.set(TokenFlags::UNTERMINATED);
        }
        self.pos = end;
        self.at_bol = bol;
        Token {
            start: start as u32,
            len: (end - start) as u32,
            kind: TokenKind::Comment,
            keyword: Keyword::None,
            flags,
        }
    }
}

impl Iterator for Lexer<'_> {
    type Item = Token;

    #[inline]
    fn next(&mut self) -> Option<Token> {
        self.next_token()
    }
}

/// Tokenizes `src` into `out` (cleared first), reusing its allocation.
pub fn lex_into(src: &str, out: &mut Vec<Token>) {
    out.clear();
    out.reserve(src.len() / 5);
    out.extend(Lexer::new(src));
}

#[cfg(test)]
mod tests {
    use super::Keyword;
    use super::*;
    use TokenKind::*;

    fn lex(src: &str) -> Vec<(TokenKind, &str)> {
        Lexer::new(src).map(|t| (t.kind, t.text(src))).collect()
    }

    fn lex_c(src: &str) -> Vec<(TokenKind, &str)> {
        Lexer::new(src).emit_comments(true).map(|t| (t.kind, t.text(src))).collect()
    }

    #[test]
    fn declaration() {
        assert_eq!(
            lex("class Foo : public Bar { int x; };"),
            [
                (Keyword, "class"),
                (Ident, "Foo"),
                (Colon, ":"),
                (Keyword, "public"),
                (Ident, "Bar"),
                (LBrace, "{"),
                (Ident, "int"),
                (Ident, "x"),
                (Semi, ";"),
                (RBrace, "}"),
                (Semi, ";"),
            ]
        );
    }

    #[test]
    fn keywords_are_exact() {
        let t: Vec<_> = Lexer::new("virtual override overrides final_ Final").collect();
        assert_eq!(t[0].keyword, Keyword::Virtual);
        assert_eq!(t[1].keyword, Keyword::Override);
        assert_eq!((t[2].kind, t[3].kind, t[4].kind), (Ident, Ident, Ident));
    }

    #[test]
    fn greater_than_is_always_single() {
        let k: Vec<_> = lex("a<b<c>> d>=e").into_iter().map(|t| t.0).collect();
        assert_eq!(k, [Ident, Lt, Ident, Lt, Ident, Gt, Gt, Ident, Gt, Eq, Ident]);
    }

    #[test]
    fn operators() {
        let src = ":: ... -> ->* .* ++ -- += <<= << <= <=> == != && || ## # ~ ? . - >";
        let k: Vec<_> = lex(src).into_iter().map(|t| t.0).collect();
        assert_eq!(
            k,
            [
                ColonColon, Ellipsis, Arrow, Op, Op, Op, Op, Op, Op, Shl, Le, Op, EqEq, NotEq, AmpAmp,
                PipePipe, HashHash, Hash, Tilde, Question, Dot, Minus, Gt
            ]
        );
    }

    #[test]
    fn numbers() {
        for n in ["42", "0x1F", "1'000'000", "1.5e-3f", "0x1p+4", ".5", "1.", "10ull", "0b1010"] {
            assert_eq!(lex(n), [(Number, n)], "{n}");
        }
        assert_eq!(lex("a.b"), [(Ident, "a"), (Dot, "."), (Ident, "b")]);
        assert_eq!(lex("1+2"), [(Number, "1"), (Plus, "+"), (Number, "2")]);
        assert_eq!(lex("x[1]"), [(Ident, "x"), (LBracket, "["), (Number, "1"), (RBracket, "]")]);
    }

    #[test]
    fn strings_and_chars() {
        assert_eq!(lex(r#""a\"b" 'c' '\'' L"w" u8"x" U'y' u8'z'"#).len(), 7);
        for s in [r#""a\"b""#, r#"L"wide""#, r#"u8"x""#, "'\\''", "L'c'"] {
            assert_eq!(lex(s).len(), 1, "{s}");
        }
        assert_eq!(lex(r#"u8 "x""#), [(Ident, "u8"), (String, "\"x\"")]);
        assert_eq!(lex(r#"foo"x""#), [(Ident, "foo"), (String, "\"x\"")]);
        assert_eq!(lex(r#"R'x'"#), [(Ident, "R"), (Char, "'x'")]);
    }

    #[test]
    fn raw_strings() {
        let src = r#"R"xy(a)" "quoted" )xy" next"#;
        assert_eq!(lex(src), [(String, r#"R"xy(a)" "quoted" )xy""#), (Ident, "next")]);
        assert_eq!(lex(r#"LR"(a // b)" x"#), [(String, r#"LR"(a // b)""#), (Ident, "x")]);
        let t = Lexer::new("R\"x(never closed").next().unwrap();
        assert!(t.unterminated());
        // Malformed delimiter degrades to an ordinary string instead of panicking.
        assert_eq!(lex("R\"a b(\" z")[0].0, String);
    }

    #[test]
    fn unterminated_string_stops_at_newline() {
        let src = "\"abc\nint x;";
        let toks: Vec<_> = Lexer::new(src).collect();
        assert!(toks[0].unterminated());
        assert_eq!(toks[0].text(src), "\"abc");
        assert_eq!(toks[1].text(src), "int");
        assert!(toks[1].at_line_start());
    }

    #[test]
    fn escaped_newline_continues_string() {
        let src = "\"a\\\nb\" x";
        assert_eq!(lex(src), [(String, "\"a\\\nb\""), (Ident, "x")]);
        let src = "\"a\\\r\nb\" x";
        assert_eq!(lex(src)[0].1, "\"a\\\r\nb\"");
    }

    #[test]
    fn comments_skipped_by_default() {
        assert_eq!(lex("a // c\n b /* x\ny */ c"), [(Ident, "a"), (Ident, "b"), (Ident, "c")]);
        assert_eq!(lex("a /**/ b /***/ c"), [(Ident, "a"), (Ident, "b"), (Ident, "c")]);
        assert_eq!(lex("/*/ x"), []);
    }

    #[test]
    fn comments_emitted_on_request() {
        assert_eq!(
            lex_c("a // c\n/* d */ b"),
            [(Ident, "a"), (Comment, "// c"), (Comment, "/* d */"), (Ident, "b")]
        );
        let t = Lexer::new("/* open").emit_comments(true).next().unwrap();
        assert!(t.unterminated());
    }

    #[test]
    fn line_comment_splice() {
        assert_eq!(lex("a // c \\\n still comment\nb"), [(Ident, "a"), (Ident, "b")]);
        assert_eq!(lex("a // c \\\r\n still\r\nb"), [(Ident, "a"), (Ident, "b")]);
        // The last backslash before the newline is a splice, whatever precedes it.
        assert_eq!(lex("a // c \\\\\nb"), [(Ident, "a")]);
    }

    #[test]
    fn bol_and_space_flags() {
        let src = "#define A 1\n  #  if X\nint /* c */ y; /* multi\nline */ #pragma";
        let toks: Vec<_> = Lexer::new(src).collect();
        let bol: Vec<_> = toks.iter().filter(|t| t.at_line_start()).map(|t| t.text(src)).collect();
        // The multi-line comment is just a space: the trailing `#` is mid-line.
        assert_eq!(bol, ["#", "#", "int"]);
        assert!(toks[2].space_before() && !toks[0].space_before());
        let define = &toks[1];
        assert_eq!(define.text(src), "define");
        assert!(!define.space_before() && !define.at_line_start());
    }

    #[test]
    fn bol_survives_comment_without_newline() {
        let src = "/* c */ #define A";
        assert!(Lexer::new(src).next().unwrap().at_line_start());
        let toks: Vec<_> = Lexer::new(src).emit_comments(true).collect();
        assert!(toks[0].at_line_start() && toks[1].at_line_start());
        assert!(!toks[2].at_line_start());
    }

    #[test]
    fn multiline_comment_does_not_split_directive() {
        let toks: Vec<_> = Lexer::new("#define X /* a\nb */ 5\nY").collect();
        let bol: Vec<_> = toks.iter().map(|t| t.at_line_start()).collect();
        assert_eq!(bol, [true, false, false, false, true]);
        let toks: Vec<_> = Lexer::new("/* a\nb */ #define X").collect();
        assert!(toks[0].at_line_start());
    }

    #[test]
    fn line_splice_between_tokens() {
        let src = "#define A \\\n  1\nB";
        let toks: Vec<_> = Lexer::new(src).collect();
        assert_eq!(toks.len(), 5);
        assert!(!toks[3].at_line_start());
        assert!(toks[4].at_line_start());
    }

    #[test]
    fn unicode_identifiers_and_stray_bytes() {
        assert_eq!(lex("größe é"), [(Ident, "größe"), (Ident, "é")]);
        assert_eq!(lex("a @ b"), [(Ident, "a"), (Unknown, "@"), (Ident, "b")]);
        assert_eq!(lex("\\ x"), [(Unknown, "\\"), (Ident, "x")]);
    }

    #[test]
    fn empty_and_whitespace_only() {
        assert!(lex("").is_empty());
        assert!(lex(" \r\n\t \x0c").is_empty());
    }

    #[test]
    fn macro_heavy_code() {
        let k: Vec<_> = lex(r#"UCLASS(Blueprintable, meta=(Foo="x")) class A_API Foo {}"#)
            .into_iter()
            .map(|t| t.0)
            .collect();
        assert_eq!(&k[..2], [Ident, LParen]);
        assert_eq!(k.iter().filter(|&&k| k == Keyword).count(), 1);
    }

    #[test]
    fn lex_into_reuses_buffer() {
        let mut v = Vec::new();
        lex_into("a b c", &mut v);
        assert_eq!(v.len(), 3);
        let cap = v.capacity();
        lex_into("x", &mut v);
        assert_eq!(v.len(), 1);
        assert!(v.capacity() >= cap);
    }
}
