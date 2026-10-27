//! Property-style robustness tests: the lexer must never panic, must always
//! make progress, and its tokens must tile the source exactly.

use lightspeed::testgen::{Rng, cpp_source};
use lightspeed::{Lexer, SourceFile, Token, TokenKind};

const FRAGMENTS: &[&str] = &[
    "/*", "*/", "//", "\\\n", "\\\r\n", "\\", "R\"x(", ")x\"", "R\"(", ")\"", "\"", "'", "L\"", "u8'", "\n", "\r\n", "\t",
    " ", "#", "##", "0x1e+5", "1'", "..", "...", "<<=", ">>", "->*", "é", "日本", "\u{feff}", "\0", "\x7f", "@", "`",
    "class", "virtual", "override", "{", "}", "(", ")", "<", ">", "::", "ident", "_", "$", "9", ".", "e+", "R", "u8",
];

fn random_source(rng: &mut Rng) -> String {
    let n = rng.below(60);
    (0..n).map(|_| FRAGMENTS[rng.below(FRAGMENTS.len())]).collect()
}

/// With comments emitted, the gaps between tokens may only be whitespace or
/// line splices; anything else was silently dropped by the lexer.
fn assert_tiles(src: &str, toks: &[Token]) {
    let mut pos = 0usize;
    for t in toks {
        assert!(t.len > 0, "empty token {t:?} in {src:?}");
        let (s, e) = (t.start as usize, t.end() as usize);
        assert!(s >= pos, "overlap/out of order at {t:?} in {src:?}");
        assert!(e <= src.len(), "token past EOF in {src:?}");
        assert!(src.is_char_boundary(s) && src.is_char_boundary(e), "split char in {src:?}");
        let gap = &src[pos..s];
        let cleaned = gap.replace("\\\r\n", "").replace("\\\n", "");
        assert!(
            cleaned.bytes().all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x0B | 0x0C)),
            "dropped non-trivia {gap:?} before {t:?} in {src:?}"
        );
        pos = e;
    }
    let tail = &src[pos..];
    let cleaned = tail.replace("\\\r\n", "").replace("\\\n", "");
    assert!(cleaned.bytes().all(|b| b.is_ascii_whitespace() || b == 0x0B), "dropped tail {tail:?}");
}

#[test]
fn fuzz_fragments_never_panic_and_tile() {
    let mut rng = Rng::new(0xC0FFEE);
    for _ in 0..20_000 {
        let src = random_source(&mut rng);
        let toks: Vec<_> = Lexer::new(&src).emit_comments(true).collect();
        assert_tiles(&src, &toks);
        // Without comments the stream is the same minus comment tokens.
        let plain: Vec<_> = Lexer::new(&src).collect();
        let filtered: Vec<_> = toks.iter().filter(|t| t.kind != TokenKind::Comment).map(|t| (t.start, t.len, t.kind)).collect();
        let plain: Vec<_> = plain.iter().map(|t| (t.start, t.len, t.kind)).collect();
        assert_eq!(plain, filtered, "comment toggle changed tokens for {src:?}");
    }
}

#[test]
fn fuzz_every_prefix_of_generated_code() {
    let src = cpp_source(6_000, 7);
    for end in (0..src.len()).filter(|&i| src.is_char_boundary(i)).step_by(7) {
        let part = &src[..end];
        let toks: Vec<_> = Lexer::new(part).emit_comments(true).collect();
        assert_tiles(part, &toks);
    }
}

#[test]
fn generated_code_has_expected_shape() {
    let src = cpp_source(200_000, 1);
    assert!(src.len() >= 200_000);
    let toks: Vec<_> = Lexer::new(&src).emit_comments(true).collect();
    assert_tiles(&src, &toks);
    assert!(toks.iter().all(|t| !t.unterminated()), "generator must emit well-formed C++");
    let kw = toks.iter().filter(|t| t.keyword == lightspeed::Keyword::Virtual).count();
    assert!(kw > 100);
    // Every directive '#' is at line start.
    for t in toks.iter().filter(|t| t.kind == TokenKind::Hash) {
        assert!(t.at_line_start());
    }
    assert_eq!(cpp_source(5_000, 3), cpp_source(5_000, 3), "generator must be deterministic");
}

#[test]
fn file_to_tokens_pipeline() {
    let text = cpp_source(300_000, 2);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gen.h");
    std::fs::write(&path, &text).unwrap();

    let mapped = SourceFile::load(&path).unwrap();
    assert!(mapped.is_mapped(), "300 KB should exceed the default mmap threshold");
    assert_eq!(mapped.text(), text);

    let a: Vec<_> = Lexer::new(mapped.text()).collect();
    let b: Vec<_> = Lexer::new(&text).collect();
    assert_eq!(a, b);
    // Token text must borrow from the mapped region, not a copy.
    let base = mapped.text().as_ptr() as usize;
    let first = a[0].text(mapped.text()).as_ptr() as usize;
    assert!(first >= base && first < base + mapped.len());
}
