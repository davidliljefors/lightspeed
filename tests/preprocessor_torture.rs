//! Preprocessor torture tests: a table of nasty cases checked against hand-written
//! expectations, differentially against `clang -E` when available, plus
//! robustness, isolation and determinism checks.

use lightspeed::workspace::compile_commands::{FlagSet, IncludeDir, IncludeKind, MacroOp};
use lightspeed::lexer::{Token, lex_into};
use lightspeed::pp::vfs::MemFs;
use lightspeed::pp::{DiagKind, PpConfig, PpOutput, Preprocessor, Profile};
use rayon::prelude::*;

const MAIN: &str = "/t/main.cpp";

fn norm(s: &str) -> String {
    let mut toks: Vec<Token> = Vec::new();
    lex_into(s, &mut toks);
    let mut out = String::new();
    for t in &toks {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(t.text(s));
    }
    out
}

fn flags(dirs: &[&str]) -> FlagSet {
    FlagSet {
        include_dirs: dirs
            .iter()
            .map(|d| IncludeDir { path: (*d).to_string(), kind: IncludeKind::Normal })
            .collect(),
        std: Some("c++17".into()),
        ..FlagSet::default()
    }
}

fn cfg() -> PpConfig {
    PpConfig { follow_includes: true, ..PpConfig::default() }
}

fn fs_with(files: &[(&str, &str)]) -> MemFs {
    let mut fs = MemFs::new();
    for (p, t) in files {
        fs.add(&format!("/t/{p}"), t);
    }
    fs
}

fn run_with(files: &[(&str, &str)], fl: &FlagSet, src: &str) -> (String, PpOutput) {
    let fs = fs_with(files);
    let mut pp = Preprocessor::new(&fs, cfg());
    pp.set_flags(fl);
    let mut out = PpOutput::default();
    pp.run(MAIN, src, &mut out);
    (norm(&out.render(src)), out)
}

fn run(src: &str) -> (String, PpOutput) {
    run_with(&[], &flags(&["/t"]), src)
}

fn pp1(src: &str) -> String {
    run(src).0
}

struct Case {
    name: &'static str,
    files: &'static [(&'static str, &'static str)],
    src: &'static str,
    expect: &'static str,
    /// Also compare against clang (only when headers contain nothing but directives).
    diff: bool,
}

const fn c(
    name: &'static str,
    src: &'static str,
    expect: &'static str,
) -> Case {
    Case { name, files: &[], src, expect, diff: true }
}

const fn cf(
    name: &'static str,
    files: &'static [(&'static str, &'static str)],
    src: &'static str,
    expect: &'static str,
) -> Case {
    Case { name, files, src, expect, diff: true }
}

const fn tol(
    name: &'static str,
    src: &'static str,
    expect: &'static str,
) -> Case {
    Case { name, files: &[], src, expect, diff: false }
}

fn cases() -> Vec<Case> {
    vec![
        // ---- basics
        c("object", "#define A 1\nA A", "1 1"),
        c("empty_macro", "#define E\nint E x;", "int x ;"),
        c("keyword_macro", "#define class struct\nclass X {};", "struct X { } ;"),
        c("api_macro", "#define ENGINE_API __declspec(dllexport)\nclass ENGINE_API Foo {};",
          "class __declspec ( dllexport ) Foo { } ;"),
        c("undef_redefine", "#define A 1\n#undef A\n#define A 2\nA", "2"),
        c("redefine_same_line_use", "#define A 1\nA\n#undef A\nA", "1 A"),
        c("chain", "#define A B\n#define B C\n#define C 3\nA", "3"),
        c("late_definition", "#define A B\n#define B 5\nA\n#undef B\nA", "5 B"),
        // ---- function-like
        c("fn_basic", "#define F(x) (x+1)\nF(2)", "( 2 + 1 )"),
        c("fn_space_before_paren_is_object", "#define F (x)\nF(1)", "( x ) ( 1 )"),
        c("fn_without_call", "#define F(x) x\nint F;", "int F ;"),
        c("fn_multiline_args", "#define F(a,b) a-b\nF(1,\n2)", "1 - 2"),
        c("fn_newline_before_paren", "#define F(x) <x>\nF\n(3)", "< 3 >"),
        c("fn_empty_arg", "#define F(x) [x]\nF()", "[ ]"),
        c("fn_two_empty_args", "#define F(a,b) [a|b]\nF(,)", "[ | ]"),
        c("fn_paren_commas", "#define F(a) a\nF((1,2))", "( 1 , 2 )"),
        c("fn_nested_calls", "#define F(x) x*2\nF(F(F(1)))", "1 * 2 * 2 * 2"),
        c("fn_arg_preexpanded", "#define A 7\n#define F(x) x x\nF(A)", "7 7"),
        c("fn_arg_unused_not_needed", "#define F(a,b) a\n#define X(\nF(1,2)", "1"),
        c("fn_zero_params_call", "#define F() ok\nF() F( )", "ok ok"),
        c("fn_brackets_not_grouping", "#define F(a,b) a|b\nF([1,2])", "[ 1 | 2 ]"),
        c("fn_string_with_comma", "#define F(a) a\nF(\"x,y\")", "\"x,y\""),
        c("fn_char_with_paren", "#define F(a) a\nF(')')", "')'"),
        c("fn_in_arg_of_object", "#define G F\n#define F(x) <x>\nG(1)", "< 1 >"),
        c("fn_name_then_other_paren", "#define F(x) x\nF + (1)", "F + ( 1 )"),
        // ---- # and ##
        c("stringify", "#define S(x) #x\nS(a + b)", "\"a + b\""),
        c("stringify_spaces_collapse", "#define S(x) #x\nS(  a   +\n  b  )", "\"a + b\""),
        c("stringify_escapes", "#define S(x) #x\nS(\"a\\n\" '\\'')", "\"\\\"a\\\\n\\\" '\\\\''\""),
        c("stringify_empty", "#define S(x) #x\nS()", "\"\""),
        c("stringify_no_preexpand", "#define A 1\n#define S(x) #x\nS(A)", "\"A\""),
        c("xs_trick", "#define A 1\n#define S(x) #x\n#define XS(x) S(x)\nXS(A)", "\"1\""),
        c("paste_basic", "#define P(a,b) a##b\nP(foo,bar)", "foobar"),
        c("paste_numbers", "#define P(a,b) a##b\nP(1,2) P(1,.5)", "12 1.5"),
        c("paste_empty_lhs", "#define P(a,b) a##b\nP(,x)", "x"),
        c("paste_empty_rhs", "#define P(a,b) a##b\nP(x,)", "x"),
        c("paste_both_empty", "#define P(a,b) [a##b]\nP(,)", "[ ]"),
        c("paste_no_preexpand", "#define A 1\n#define P(a,b) a##b\nP(A,A)", "AA"),
        c("paste_then_expands", "#define AB 9\n#define P(a,b) a##b\nP(A,B)", "9"),
        c("paste_op", "#define P(a,b) a##b\nP(<,<) P(-,>)", "<< ->"),
        c("paste_object_like", "#define X a ## b\n#define ab 4\nX", "4"),
        c("paste_chain", "#define P(a,b,c) a##b##c\nP(x,y,z)", "xyz"),
        c("paste_arg_multi_token", "#define P(a,b) a##b\nP(1 2, 3 4)", "1 23 4"),
        c("paste_with_stringify", "#define S(x) #x\n#define P(a) S(a##1)\nP(v)", "\"v1\""),
        c("paste_and_stringify_same", "#define F(a,b) #a a##b\nF(x,y)", "\"x\" xy"),
        c("stringify_paste_operand", "#define F(a) #a\nF(a ## b)", "\"a ## b\""),
        // ---- variadics
        c("va_args", "#define V(...) f(__VA_ARGS__)\nV(1,2,3) V()", "f ( 1 , 2 , 3 ) f ( )"),
        c("va_after_named", "#define V(a,...) a:__VA_ARGS__\nV(1,2,3)", "1 : 2 , 3"),
        c("va_named_missing", "#define V(a,...) a:__VA_ARGS__;\nV(1)", "1 : ;"),
        c("va_gnu_comma", "#define V(a,...) f(a, ## __VA_ARGS__)\nV(1) V(1,2)", "f ( 1 ) f ( 1 , 2 )"),
        c("va_named_variadic", "#define V(args...) f(args)\nV(1,2)", "f ( 1 , 2 )"),
        c("va_opt", "#define V(a,...) f(a __VA_OPT__(,) __VA_ARGS__)\nV(1) V(1,2)", "f ( 1 ) f ( 1 , 2 )"),
        c("va_opt_paste", "#define V(...) x __VA_OPT__(##y)\nV() V(1)", "x xy"),
        c("va_count",
          "#define CNT(...) CNT_(__VA_ARGS__,5,4,3,2,1,0)\n#define CNT_(a,b,c,d,e,N,...) N\nCNT(x) CNT(x,y) CNT(x,y,z)",
          "1 2 3"),
        c("va_forward", "#define A(...) B(__VA_ARGS__)\n#define B(a,b) a+b\nA(1,2)", "1 + 2"),
        c("va_stringify", "#define S(...) #__VA_ARGS__\nS(a, b ,c)", "\"a, b ,c\""),
        // ---- recursion and painting
        c("self_ref", "#define A A\nA", "A"),
        c("self_ref_expr", "#define foo (4 + foo)\nfoo", "( 4 + foo )"),
        c("mutual", "#define a b\n#define b a\na b", "a b"),
        c("mutual3", "#define a b\n#define b c\n#define c a\na b c", "a b c"),
        c("fn_recursive", "#define f(x) f(x+1)\nf(1)", "f ( 1 + 1 )"),
        c("fn_f_of_f", "#define f(x) x\nf(f)(1)", "f ( 1 )"),
        c("fn_f1_f2", "#define f(a) a*g\n#define g(a) f(a)\nf(2)(9)", "2 * 9 * g"),
        c("empty_trick", "#define EMPTY\n#define F(x) x\nF EMPTY (1)", "F ( 1 )"),
        c("deferred_expand",
          "#define EMPTY\n#define DEFER(x) x EMPTY\n#define F() 1\nDEFER(F)()",
          "F ( )"),
        c("obstruct_eval",
          "#define EMPTY()\n#define DEFER(x) x EMPTY()\n#define EXPAND(...) __VA_ARGS__\n#define F() 1\nEXPAND(DEFER(F)())",
          "1"),
        c("call_result_called", "#define A() B\n#define B() 1\nA()()", "1"),
        c("painted_stays_painted",
          "#define f(x) x g\n#define g f\nf(1)(2)(3)",
          "1 f ( 2 ) ( 3 )"),
        // ---- the C standard's example 3
        c("std_example3",
          "#define x 3\n#define f(a) f(x * (a))\n#undef x\n#define x 2\n#define g f\n#define z z[0]\n#define h g(~\n#define m(a) a(w)\n#define w 0,1\n#define t(a) a\n#define p() int\n#define q(x) x\n#define r(x,y) x ## y\n#define str(x) # x\nf(y+1) + f(f(z)) % t(t(g)(0) + t)(1);\ng(x+(3,4)-w) | h 5) & m\n(f)^m(m);\np() i[q()] = { q(1), r(2,3), r(4,), r(,5), r(,) };\nchar c[2][6] = { str(hello), str() };",
          "f ( 2 * ( y + 1 ) ) + f ( 2 * ( f ( 2 * ( z [ 0 ] ) ) ) ) % f ( 2 * ( 0 ) ) + t ( 1 ) ; f ( 2 * ( 2 + ( 3 , 4 ) - 0 , 1 ) ) | f ( 2 * ( ~ 5 ) ) & f ( 2 * ( 0 , 1 ) ) ^ m ( 0 , 1 ) ; int i [ ] = { 1 , 23 , 4 , 5 , } ; char c [ 2 ] [ 6 ] = { \"hello\" , \"\" } ;"),
        c("std_example5",
          "#define t(x,y,z) x ## y ## z\nint j[] = { t(1,2,3), t(,4,5), t(6,,7), t(8,9,), t(10,,), t(,11,), t(,,12), t(,,) };",
          "int j [ ] = { 123 , 45 , 67 , 89 , 10 , 11 , 12 , } ;"),
        c("std_example7",
          "#define debug(...) fprintf(stderr, __VA_ARGS__)\n#define showlist(...) puts(#__VA_ARGS__)\n#define report(test, ...) ((test)?puts(#test): printf(__VA_ARGS__))\ndebug(\"Flag\");\ndebug(\"X = %d\\n\", x);\nshowlist(The first, second, and third items.);\nreport(x>y, \"x is %d but y is %d\", x, y);",
          "fprintf ( stderr , \"Flag\" ) ; fprintf ( stderr , \"X = %d\\n\" , x ) ; puts ( \"The first, second, and third items.\" ) ; ( ( x > y ) ? puts ( \"x>y\" ) : printf ( \"x is %d but y is %d\" , x , y ) ) ;"),
        c("std_example4_hash",
          "#define xstr(s) str(s)\n#define str(s) # s\n#define INCFILE(n) vers ## n\n#define glue(a, b) a ## b\n#define xglue(a, b) glue(a, b)\n#define HIGHLOW \"hello\"\n#define LOW LOW \", world\"\nglue(HIGH, LOW);\nxglue(HIGH, LOW)\nxstr(INCFILE(2).h)",
          "\"hello\" ; \"hello\" \", world\" \"vers2.h\""),
        // ---- conditionals
        c("if_basic", "#if 1\na\n#else\nb\n#endif", "a"),
        c("if_zero", "#if 0\na\n#else\nb\n#endif", "b"),
        c("ifdef_ifndef", "#define X\n#ifdef X\na\n#endif\n#ifndef X\nb\n#endif\n#ifndef Y\nc\n#endif", "a c"),
        c("elif_chain", "#define N 3\n#if N==1\na\n#elif N==2\nb\n#elif N==3\nc\n#elif N==3\nd\n#else\ne\n#endif", "c"),
        c("nested_inactive", "#if 0\n#if 1\na\n#else\nb\n#endif\n#elif 1\nc\n#endif", "c"),
        c("nested_inactive_deep", "#if 0\n#if 1\n#if 1\na\n#endif\n#endif\n#else\nz\n#endif", "z"),
        c("garbage_in_skipped", "#if 0\n#bogus\nit's an apostrophe\n\"unterminated\n#if (((\n#endif\n#else\nok\n#endif", "ok"),
        c("short_circuit_div", "#if 0 && (1/0)\nbad\n#else\nok\n#endif", "ok"),
        c("short_circuit_or", "#if 1 || (1/0)\nok\n#endif", "ok"),
        c("short_circuit_ternary", "#if 1 ? 2 : (1/0)\nok\n#endif", "ok"),
        c("defined_forms", "#define A\n#if defined(A) && defined A && !defined(B) && !defined B\nok\n#endif", "ok"),
        c("defined_via_macro", "#define A\n#define D defined(A)\n#if D\nok\n#endif", "ok"),
        c("arith", "#if (1+2)*3 == 9 && 10%3 == 1 && -5/2 == -2 && 7>>1 == 3 && 1<<4 == 16\nok\n#endif", "ok"),
        c("bitops", "#if (6&3)==2 && (6|3)==7 && (6^3)==5 && ~0 == -1 && !0\nok\n#endif", "ok"),
        c("unsigned_cmp", "#if -1 < 0u\nbad\n#else\nok\n#endif", "ok"),
        c("unsigned_big", "#if 0xFFFFFFFFFFFFFFFF > 0\nok\n#endif", "ok"),
        c("literals", "#if 0x10 == 16 && 010 == 8 && 0b11 == 3 && 1'000 == 1000 && 5UL == 5 && 5ll == 5\nok\n#endif", "ok"),
        c("char_literals", "#if 'a' == 97 && '\\n' == 10 && '\\x41' == 65 && '\\0' == 0\nok\n#endif", "ok"),
        c("unknown_ident_zero", "#if UNDEFINED_THING == 0 && !UNDEFINED_THING\nok\n#endif", "ok"),
        c("true_false", "#if true && !false\nok\n#endif", "ok"),
        c("fn_macro_in_if", "#define MAX(a,b) ((a)>(b)?(a):(b))\n#if MAX(3,4) == 4\nok\n#endif", "ok"),
        c("macro_in_if", "#define V 5\n#define W (V*2)\n#if W == 10\nok\n#endif", "ok"),
        c("ternary_nested", "#if (1 ? 0 : 1 ? 2 : 3) == 0 && (0 ? 1 : 0 ? 2 : 3) == 3\nok\n#endif", "ok"),
        c("comma_in_if", "#if (1, 2) == 2\nok\n#endif", "ok"),
        c("cplusplus_17", "#if __cplusplus == 201703L\nok\n#endif", "ok"),
        c("comment_in_directive", "#if /* a\nb */ 1 // x\nok\n#endif", "ok"),
        c("define_multiline_comment", "#define X /* a\nb */ 5\nX", "5"),
        c("line_continuation", "#define L(a,b) \\\n  a + \\\n  b\nL(1,2)", "1 + 2"),
        c("line_continuation_marker_unused", "#define L(a) \\\n a\nL(1)", "1"),
        c("null_and_spaced_directives", "#\n   #   define   S   9\n#\nS", "9"),
        c("junk_after_else_endif", "#if 1\na\n#else junk\nb\n#endif junk\nc", "a c"),
        c("unevaluated_elif", "#if 1\na\n#elif 1/0\nb\n#endif", "a"),
        c("if_with_defined_and_not_def", "#if defined(__cplusplus) && !defined(__STDC_VERSION__)\nok\n#endif", "ok"),
        c("else_if_nesting_in_taken", "#if 1\n#if 0\na\n#elif 0\nb\n#else\nc\n#endif\n#endif", "c"),
        // ---- directives in unusual places
        c("directive_in_macro_args", "#define F(a,b) a+b\nF(1,\n#define Q 2\nQ)", "1 + 2"),
        c("ifdef_in_macro_args", "#define F(a) [a]\nF(\n#if 1\nyes\n#else\nno\n#endif\n)", "[ yes ]"),
        c("hash_in_string", "const char* s = \"#define X 1\";\nX", "const char * s = \"#define X 1\" ; X"),
        c("hash_in_raw_string", "const char* s = R\"(\n#define X 1\n)\";\nX", "const char * s = R\"(\n#define X 1\n)\" ; X"),
        c("hash_in_comment", "/*\n#define X 1\n*/\nX", "X"),
        c("hash_not_at_bol", "a # define X 1\nX", "a # define X 1 X"),
        c("hash_after_block_comment", "/* c */ #define X 1\nX", "1"),
        c("hash_after_line_comment_cont", "int a; // c\n#define X 1\nX", "int a ; 1"),
        // ---- builtins
        c("line", "a __LINE__\n\n__LINE__", "a 1 3"),
        c("line_in_macro", "#define L __LINE__\n\n\nL", "4"),
        c("line_multiline_call", "#define F(x) x __LINE__\nF(\n1\n)", "1 4"),
        c("counter", "__COUNTER__ __COUNTER__ __COUNTER__", "0 1 2"),
        c("include_level_main", "__INCLUDE_LEVEL__", "0"),
        cf("include_level_header", &[("h.h", "#if __INCLUDE_LEVEL__ == 1\n#define LV 1\n#endif\n")], "#include \"h.h\"\nLV", "1"),
        tol("file", "__FILE__", "\"/t/main.cpp\""),
        // ---- includes
        cf("include_define_visible", &[("h.h", "#define H 42\n")], "#include \"h.h\"\nH", "42"),
        cf("include_angled", &[("h.h", "#define H 42\n")], "#include <h.h>\nH", "42"),
        cf("include_macro_form", &[("h.h", "#define H 42\n")], "#define HDR \"h.h\"\n#include HDR\nH", "42"),
        cf("include_macro_angle", &[("h.h", "#define H 42\n")], "#define HDR <h.h>\n#include HDR\nH", "42"),
        cf("include_dotdot", &[("x.h", "#define X 1\n"), ("sub/y.h", "#define Y 2\n")], "#include \"sub/../x.h\"\n#include \"sub/y.h\"\nX Y", "1 2"),
        cf("include_relative_to_includer", &[("sub/a.h", "#include \"b.h\"\n"), ("sub/b.h", "#define B 7\n")], "#include \"sub/a.h\"\nB", "7"),
        cf("guard_ifndef", &[("g.h", "#ifndef G_H\n#define G_H\n#define CNT_G 1\n#endif\n")], "#include \"g.h\"\n#include \"g.h\"\nCNT_G", "1"),
        cf("guard_reentered_after_undef", &[("g.h", "#ifndef G_H\n#define G_H\n#define N 1\n#endif\n")], "#include \"g.h\"\n#undef G_H\n#undef N\n#include \"g.h\"\nN", "1"),
        cf("include_in_inactive", &[("h.h", "#define H 1\n")], "#if 0\n#include \"h.h\"\n#endif\nH", "H"),
        cf("include_conditional_flip", &[("h.h", "#undef M\n#define M 2\n")], "#define M 1\n#include \"h.h\"\nM", "2"),
        cf("header_ifdef_balance", &[("h.h", "#ifdef NOPE\n#define X 1\n#else\n#define X 2\n#endif\n")], "#include \"h.h\"\nX", "2"),
        cf("include_computed_into_if", &[("v.h", "#define VER 3\n")], "#include \"v.h\"\n#if VER >= 3\nnew\n#else\nold\n#endif", "new"),
        cf("has_include_true", &[("h.h", "")], "#if __has_include(\"h.h\")\nyes\n#else\nno\n#endif", "yes"),
        cf("has_include_false", &[], "#if __has_include(<nope.h>)\nyes\n#else\nno\n#endif", "no"),
        cf("has_include_defined", &[], "#ifdef __has_include\nyes\n#endif", "yes"),
        cf("has_include_guarded_include", &[("h.h", "#define H 1\n")], "#if __has_include(\"h.h\")\n#include \"h.h\"\n#endif\nH", "1"),
        cf("has_feature_zero", &[], "#if __has_feature(x) || __has_builtin(y) || __has_attribute(z)\nbad\n#else\nok\n#endif", "ok"),
        // ---- tolerance: deliberate deviations from the standard
        tol("invalid_paste", "#define P(a,b) a##b\nP(+,-)", "+ -"),
        tol("unterminated_if", "#if 1\na", "a"),
        tol("unterminated_if_false", "#if 0\na\nb", ""),
        tol("stray_endif", "a\n#endif\nb", "a b"),
        tol("stray_else", "a\n#else\nb", "a b"),
        tol("push_pop_macro",
            "#define A 1\n#pragma push_macro(\"A\")\n#undef A\n#define A 2\nA\n#pragma pop_macro(\"A\")\nA",
            "2 1"),
        tol("pragma_once_dropped", "#pragma once\na", "a"),
        tol("pragma_operator_dropped", "a _Pragma(\"x\") b", "a b"),
        tol("elifdef", "#define A\n#if 0\nx\n#elifdef A\nok\n#endif", "ok"),
        tol("error_is_diag_not_fatal", "#error stop\na", "a"),
        tol("unterminated_args_eof", "#define F(x) x\nF(1,\n", "1"),
        tol("bad_define_ignored", "#define\n#define 3 4\n#define F(\na", "a"),
        tol("bad_if_is_false", "#if (\nx\n#else\ny\n#endif", "y"),
        tol("empty_if_is_false", "#if\nx\n#else\ny\n#endif", "y"),
        tol("unknown_directive", "#frobnicate 1 2\na", "a"),
        tol("line_directive_ignored", "#line 100\na __LINE__", "a 2"),
        tol("linemarker_ignored", "# 12 \"foo.cpp\"\na", "a"),
    ]
}

/// Editing keeps the include prologue's preprocessor state in a snapshot; every
/// result must equal a from-scratch run, whatever part of the file is edited.
#[test]
fn snapshot_resume_matches_fresh_runs_across_edits() {
    let files = [
        ("a.h", "#pragma once\n#define A 1\n#define FN(x) x+A\n#ifdef GATE\n#define GATED 7\n#endif\n"),
        ("b.h", "#ifndef B_H\n#define B_H\n#include \"a.h\"\n#define B A\n#endif\n"),
    ];
    let fs = fs_with(&files);
    let fl = flags(&["/t"]);
    let mut warm = Preprocessor::new(&fs, cfg());
    warm.set_flags(&fl);
    warm.set_snapshotting(true);
    let mut fresh = Preprocessor::new(&fs, cfg());
    fresh.set_flags(&fl);

    let prologue = "#include \"b.h\"\n#define LOCAL 5 // c\n";
    let base = format!("{prologue}\nint x = FN(B) + LOCAL;\n");
    let edits: Vec<String> = vec![
        base.clone(),
        base.clone(),
        // Body edits and a new first declaration after the prologue.
        format!("{prologue}int first;\nint x = FN(B) + LOCAL;\n"),
        format!("{prologue}int first;\nint x = FN(B) + LOCAL; int y;\n"),
        // The prologue itself changes in every way.
        "#include \"b.h\"\n#define LOCAL 55 // c\n\nint x = FN(B) + LOCAL;\n".to_string(),
        "#include \"b.h\"\n#define LOCAL 5 + 1\n\nint x = FN(B) + LOCAL;\n".to_string(),
        format!("#define GATE 1\n{prologue}\nint x = FN(B) + GATED;\n"),
        format!("int before;\n{prologue}\nint x = FN(B) + LOCAL;\n"),
        format!("{prologue}#undef LOCAL\nint x = LOCAL;\n"),
        format!("{prologue}#if LOCAL == 5\nint yes;\n#else\nint no;\n#endif\n"),
        format!("{prologue}#if LOCAL == 6\nint yes;\n#else\nint no;\n#endif\n"),
        // A directive line extended in place, and a conditional wrapping the file.
        "#include \"b.h\"\n#define LOCAL 5\nint a;\n".to_string(),
        "#include \"b.h\"\n#define LOCAL 5 + A\nint a = LOCAL;\n".to_string(),
        "#ifndef G\n#define G\n#include \"b.h\"\nint a = B;\n#endif\n".to_string(),
        "#ifndef G\n#define G\n#include \"b.h\"\nint a = B; int more;\n#endif\n".to_string(),
        "#ifndef G\n#define G\n#include \"b.h\"\nint a = B; int more;\n".to_string(),
        String::new(),
        base.clone(),
    ];
    let mut a = PpOutput::default();
    let mut b = PpOutput::default();
    for (i, src) in edits.iter().enumerate() {
        warm.run(MAIN, src, &mut a);
        fresh.run(MAIN, src, &mut b);
        assert_eq!(norm(&a.render(src)), norm(&b.render(src)), "edit {i}: {src:?}");
        assert_eq!(a.includes, b.includes, "edit {i} includes");
        assert_eq!(a.defines, b.defines, "edit {i} defines");
        assert_eq!(a.skipped, b.skipped, "edit {i} skipped");
        assert_eq!(a.diagnostics, b.diagnostics, "edit {i} diagnostics");
        assert_eq!(a.deps, b.deps, "edit {i} deps");
    }
}

#[test]
fn table_matches_expectations() {
    let mut fails = Vec::new();
    for case in cases() {
        let (got, _) = run_with(case.files, &flags(&["/t"]), case.src);
        let want = norm(case.expect);
        if got != want {
            fails.push(format!("[{}]\n  got:  {got}\n  want: {want}", case.name));
        }
    }
    assert!(fails.is_empty(), "{} failing:\n{}", fails.len(), fails.join("\n"));
}

// ---------------------------------------------------------------- clang oracle

fn clang_exe() -> Option<std::path::PathBuf> {
    let candidates = [
        std::path::PathBuf::from(r"C:\Program Files\LLVM\bin\clang.exe"),
        std::path::PathBuf::from("clang"),
    ];
    candidates.into_iter().find(|p| {
        std::process::Command::new(p)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

fn clang_pp(exe: &std::path::Path, files: &[(&str, &str)], src: &str, tag: &str) -> Option<String> {
    let dir = std::env::temp_dir().join(format!("lightspeed_pp_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    for (p, t) in files {
        let f = dir.join(p);
        std::fs::create_dir_all(f.parent()?).ok()?;
        std::fs::write(f, t).ok()?;
    }
    let main = dir.join("main.cpp");
    std::fs::write(&main, src).ok()?;
    let out = std::process::Command::new(exe)
        .args(["-E", "-P", "-x", "c++", "-std=c++17", "-undef", "-nostdinc", "-nostdinc++", "-w"])
        .arg(format!("-I{}", dir.display()))
        .arg(&main)
        .output()
        .ok()?;
    let _ = std::fs::remove_dir_all(&dir);
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let kept: Vec<&str> = text.lines().filter(|l| !l.trim_start().starts_with("#pragma")).collect();
    Some(kept.join("\n"))
}

#[test]
fn differential_against_clang() {
    let Some(exe) = clang_exe() else {
        eprintln!("clang not found; skipping differential test");
        return;
    };
    let mut fails = Vec::new();
    let mut compared = 0;
    for (i, case) in cases().into_iter().enumerate() {
        if !case.diff || case.src.contains("__COUNTER__") {
            continue;
        }
        // `-undef` removes __cplusplus; supply it so both sides agree.
        let src = format!("#define __cplusplus 201703L\n{}", case.src);
        let Some(want) = clang_pp(&exe, case.files, &src, &format!("c{i}")) else {
            // Cases clang rejects are not comparable.
            continue;
        };
        let (got, _) = run_with(case.files, &flags(&["/t"]), &src);
        compared += 1;
        if got != norm(&want) {
            fails.push(format!("[{}]\n  ours:  {got}\n  clang: {}", case.name, norm(&want)));
        }
    }
    assert!(compared > 100, "only {compared} cases compared");
    assert!(fails.is_empty(), "{} differ from clang:\n{}", fails.len(), fails.join("\n"));
}

// ---------------------------------------------------------------- dedicated tests

#[test]
fn include_next_walks_dir_chain() {
    let fs = MemFs::new()
        .with("/d1/w.h", "#define FROM 1\n#include_next <w.h>\n")
        .with("/d2/w.h", "#define FROM2 2\n");
    let mut pp = Preprocessor::new(&fs, cfg());
    pp.set_flags(&flags(&["/d1", "/d2"]));
    let mut out = PpOutput::default();
    let src = "#include <w.h>\nFROM FROM2";
    pp.run(MAIN, src, &mut out);
    assert_eq!(norm(&out.render(src)), "1 2");
}

#[test]
fn pragma_once_and_guard_stats() {
    let files = [
        ("once.h", "#pragma once\n#define O 1\n"),
        ("guard.h", "#ifndef GUARD_H\n#define GUARD_H\n#define G 1\n#endif\n"),
        ("guard2.h", "#if !defined(G2)\n#define G2\n#endif\n"),
    ];
    let src = "#include \"once.h\"\n#include \"once.h\"\n#include \"guard.h\"\n#include \"guard.h\"\n#include \"guard.h\"\nO G";
    let (text, out) = run_with(&files, &flags(&["/t"]), src);
    assert_eq!(text, "1 1");
    assert_eq!(out.stats.skipped_once, 1);
    assert_eq!(out.stats.skipped_guard, 2);
    assert_eq!(out.stats.headers_entered, 2);
}

#[test]
fn header_conditionals_do_not_leak() {
    let files = [("open.h", "#if 1\n#define IN 1\n"), ("stray.h", "#endif\n#define AFTER 1\n")];
    let src = "#if 1\n#include \"open.h\"\n#include \"stray.h\"\nA\n#endif\nIN AFTER";
    let (text, out) = run_with(&files, &flags(&["/t"]), src);
    assert_eq!(text, "A 1 1");
    assert!(out.stats.header_diags >= 2);
    assert!(!out.has_diag(DiagKind::UnterminatedIf), "header's open #if must not be attributed to main");
}

#[test]
fn recursive_include_terminates() {
    let files = [("r.h", "#include \"r.h\"\n#define R 1\n")];
    let (text, out) = run_with(&files, &flags(&["/t"]), "#include \"r.h\"\nR");
    assert_eq!(text, "1");
    assert!(out.stats.header_diags > 0);
}

#[test]
fn mutual_recursive_includes_terminate() {
    let files = [("a.h", "#include \"b.h\"\n"), ("b.h", "#include \"a.h\"\n")];
    let (_, out) = run_with(&files, &flags(&["/t"]), "#include \"a.h\"\nx");
    assert!(out.stats.header_diags > 0);
}

#[test]
fn missing_include_diag_and_record() {
    let (text, out) = run("#include \"nope.h\"\n#include <also/nope.h>\nx");
    assert_eq!(text, "x");
    assert_eq!(out.stats.unresolved, 2);
    assert!(out.has_diag(DiagKind::IncludeNotFound));
    assert_eq!(out.includes.len(), 2);
    assert_eq!(out.include_name(&out.includes[0]), "nope.h");
    assert!(!out.includes[0].angled);
    assert!(out.includes[1].angled);
    assert_eq!(out.include_path(&out.includes[0]), None);
}

#[test]
fn include_records_have_resolved_paths() {
    let files = [("h.h", ""), ("sub/i.h", "")];
    let (_, out) = run_with(&files, &flags(&["/t"]), "#include \"h.h\"\n#include <sub/i.h>\n");
    assert_eq!(out.includes.len(), 2);
    assert_eq!(out.include_path(&out.includes[0]), Some("/t/h.h"));
    assert_eq!(out.include_path(&out.includes[1]), Some("/t/sub/i.h"));
}

#[test]
fn not_following_includes_still_records() {
    let fs = fs_with(&[("h.h", "#define H 1\n")]);
    let mut pp = Preprocessor::new(&fs, PpConfig { follow_includes: false, ..PpConfig::default() });
    pp.set_flags(&flags(&["/t"]));
    let mut out = PpOutput::default();
    let src = "#include \"h.h\"\nH";
    pp.run(MAIN, src, &mut out);
    assert_eq!(norm(&out.render(src)), "H");
    assert_eq!(out.includes.len(), 1);
    assert_eq!(out.include_path(&out.includes[0]), Some("/t/h.h"));
}

#[test]
fn define_records() {
    let (_, out) = run("#define A 1\n#define F(x) x\n#undef A\n");
    assert_eq!(out.defines.len(), 3);
    assert!(!out.defines[0].function_like && !out.defines[0].undef);
    assert!(out.defines[1].function_like);
    assert!(out.defines[2].undef);
    let src = "#define A 1\n#define F(x) x\n#undef A\n";
    assert_eq!(&src[out.defines[1].name_loc as usize..][..out.defines[1].name_len as usize], "F");
}

#[test]
fn skipped_ranges_cover_inactive_blocks() {
    let src = "a\n#if 0\nhidden\n#endif\nb\n";
    let (text, out) = run(src);
    assert_eq!(text, "a b");
    assert_eq!(out.skipped.len(), 1);
    let (s, e) = (out.skipped[0].0 as usize, out.skipped[0].1 as usize);
    assert!(src[s..e].contains("hidden"));
    assert!(!src[s..e].contains('b'));
}

#[test]
fn token_locations_in_bounds_and_provenance() {
    let src = "#define A foo\nint A = 1;\n";
    let (_, out) = run(src);
    for t in &out.tokens {
        assert!(t.loc as usize <= src.len());
        let s = out.text(t, src);
        assert!(!s.is_empty());
    }
    let foo = out.tokens.iter().find(|t| out.text(t, src) == "foo").unwrap();
    assert!(foo.pooled() || foo.from_macro());
    assert_eq!(&src[foo.loc as usize..][..1], "A", "macro-body tokens map to the invocation name");
}

#[test]
fn macro_args_keep_original_locations() {
    let src = "#define F(x) [x]\nF(zz)\n";
    let (_, out) = run(src);
    let zz = out.tokens.iter().find(|t| out.text(t, src) == "zz").unwrap();
    assert_eq!(&src[zz.loc as usize..][..2], "zz");
}

// ---- dependency tracking

#[test]
fn header_line_numbers_survive_the_cache() {
    let fs = fs_with(&[(
        "l.h",
        "int a;\nint b;\n#if __LINE__ == 3\n#define L3\n#endif\nint c = \\\n 1;\n#if __LINE__ == 8\n#define L8\n#endif\n",
    )]);
    let mut pp = Preprocessor::new(&fs, cfg());
    pp.set_flags(&flags(&["/t"]));
    let mut out = PpOutput::default();
    let src = "#include \"l.h\"\n#ifdef L3\n#ifdef L8\nok\n#endif\n#endif\n";
    for _ in 0..2 {
        pp.run(MAIN, src, &mut out);
        assert_eq!(norm(&out.render(src)), "ok");
    }
}

/// Known limitation: the lexer does not join a line splice that falls inside
/// a directive name, so `#de\<newline>fine` is treated as an unknown directive.
#[test]
#[ignore = "lexer does not handle line splices inside identifiers"]
fn splice_inside_directive_name() {
    assert_eq!(pp1("#de\\\nfine Z 3\nZ"), "3");
    assert_eq!(pp1("#define LO\\\nNG 5\nLONG"), "5");
}

#[test]
fn header_cache_is_transparent_and_hits() {
    let fs = fs_with(&[
        ("a.h", "#pragma once\n#include \"b.h\"\nint junk = 1; /* #define NOPE 1 */\n#define A B_VAL\n"),
        ("b.h", "#ifndef B_H\n#define B_H\nstruct S { int x; };\n#if __INCLUDE_LEVEL__ == 2\n#define B_VAL 7\n#endif\n#endif\n"),
    ]);
    let src = "#include \"a.h\"\n#include \"b.h\"\nA NOPE\n";
    let fl = flags(&["/t"]);
    let go = |budget: usize| {
        let mut pp = Preprocessor::new(&fs, PpConfig { header_cache_bytes: budget, ..cfg() });
        pp.set_flags(&fl);
        let mut out = PpOutput::default();
        let mut texts = Vec::new();
        let mut hits = 0;
        for _ in 0..3 {
            pp.run(MAIN, src, &mut out);
            texts.push(norm(&out.render(src)));
            hits += out.stats.header_cache_hits;
        }
        (texts, hits)
    };
    let (cached, hits) = go(1 << 20);
    let (uncached, no_hits) = go(0);
    assert_eq!(cached, uncached);
    assert_eq!(cached[0], "7 NOPE");
    assert!(hits >= 4, "later runs should reuse cached headers, got {hits}");
    assert_eq!(no_hits, 0);
}

fn key(p: &str) -> u64 {
    lightspeed::pp::vfs::path_key(p)
}

#[test]
fn deps_record_hits_misses_and_cached_resolutions() {
    let fs = fs_with(&[("a.h", "#include \"b.h\"\n"), ("u/b.h", "#define B 1\n")]);
    let mut f = flags(&["/t", "/t/u"]);
    f.force_includes.push("/t/pre.h".into());
    let mut pp = Preprocessor::new(&fs, cfg());
    pp.set_flags(&f);
    let mut out = PpOutput::default();
    let src = "#include \"a.h\"\n#include <nope.h>\n#if __has_include(<maybe.h>)\n#endif\n";
    let mut first = Vec::new();
    for round in 0..2 {
        pp.run(MAIN, src, &mut out);
        if round == 0 {
            first = out.deps.clone();
        }
        assert_eq!(out.deps, first, "cached resolution must report the same deps");
        assert!(out.deps.windows(2).all(|w| w[0] < w[1]), "sorted and unique");
        // Hits depend on the exact file, misses on a file of that name appearing.
        for p in [MAIN, "/t/a.h", "/t/u/b.h", "/t/pre.h"] {
            assert!(out.deps.contains(&key(p)), "missing dep {p}");
        }
        for p in ["/t/b.h", "/t/nope.h", "/t/u/nope.h", "/t/maybe.h"] {
            assert!(out.deps.contains(&lightspeed::pp::vfs::name_key(p)), "missing name dep {p}");
        }
        // Probing stops at the first hit.
        assert!(!out.deps.contains(&key("/t/u/a.h")));
        assert!(!out.deps.contains(&key("/t/nope.h")), "misses are not recorded per directory");
    }
}

#[test]
fn depgraph_invalidates_on_modify_delete_and_create() {
    use lightspeed::workspace::depgraph::DepGraph;
    let fs = fs_with(&[("shared.h", ""), ("only1.h", "")]);
    let srcs = [
        "#include \"shared.h\"\n#include \"only1.h\"\n",
        "#include \"shared.h\"\n#include \"later.h\"\n",
        "int x;\n",
    ];
    let fl = flags(&["/t"]);
    let mut pp = Preprocessor::new(&fs, cfg());
    pp.set_flags(&fl);
    let mut out = PpOutput::default();
    let mut g = DepGraph::new();
    for (i, s) in srcs.iter().enumerate() {
        pp.run(&format!("/t/tu{i}.cpp"), s, &mut out);
        g.set_deps(i as u32, &out.deps);
    }
    let q = |p: &str| {
        let mut v = Vec::new();
        g.dependents(key(p), &mut v);
        let mut w = Vec::new();
        g.dependents(lightspeed::pp::vfs::name_key(p), &mut w);
        v.extend(w);
        v.sort_unstable();
        v.dedup();
        v
    };
    assert_eq!(q("/t/shared.h"), [0, 1]);
    assert_eq!(q("/t/only1.h"), [0]);
    assert_eq!(q("/t/later.h"), [1], "a missing include must be watched for creation");
    assert_eq!(q("/t/tu2.cpp"), [2]);
}

#[test]
fn shared_force_include_prologue_matches_inline_and_keeps_deps() {
    use std::sync::Arc;
    let fs = fs_with(&[("pre.h", "#pragma once\n#include \"inner.h\"\n#define PRE INNER+1\n"), ("inner.h", "#define INNER 7\n")]);
    let mut f = flags(&["/t"]);
    f.force_includes.push("/t/pre.h".into());
    let src = "#include \"pre.h\"\nPRE __COUNTER__\n";
    let shared = Arc::new(lightspeed::pp::SharedHeaders::default());
    let mut results = Vec::new();
    for use_shared in [false, true, true] {
        let mut pp = Preprocessor::new(&fs, cfg());
        if use_shared {
            pp.set_shared(shared.clone());
        }
        pp.set_flags(&f);
        let mut out = PpOutput::default();
        pp.run(MAIN, src, &mut out);
        results.push((norm(&out.render(src)), out.deps.clone(), out.includes.len(), out.defines.len()));
    }
    assert_eq!(results[0].0, "7 + 1 0");
    assert_eq!(results[0], results[1]);
    assert_eq!(results[1], results[2]);
    assert!(results[0].1.contains(&key("/t/pre.h")) && results[0].1.contains(&key("/t/inner.h")));
}

// ---- flags

fn with_flags(macros: Vec<MacroOp>, std: Option<&str>, src: &str) -> String {
    let mut f = flags(&["/t"]);
    f.macros = macros;
    f.std = std.map(str::to_string);
    run_with(&[], &f, src).0
}

fn d(name: &str, v: &str) -> MacroOp {
    MacroOp { name: name.into(), value: Some(v.into()), undef: false }
}

#[test]
fn flag_defines() {
    let out = with_flags(
        vec![d("A", "1"), d("B", ""), d("F(x)", "x+x"), d("S", "\"str\"")],
        Some("c++17"),
        "A B F(2) S",
    );
    assert_eq!(out, "1 2 + 2 \"str\"");
}

#[test]
fn flag_undef_and_order() {
    let u = |n: &str| MacroOp { name: n.into(), value: None, undef: true };
    assert_eq!(with_flags(vec![d("A", "1"), u("A")], None, "A"), "A");
    assert_eq!(with_flags(vec![u("A"), d("A", "1")], None, "A"), "1");
    assert_eq!(with_flags(vec![d("A", "1"), d("A", "2")], None, "A"), "2");
}

#[test]
fn flag_std_sets_cplusplus() {
    assert_eq!(with_flags(vec![], Some("c++20"), "__cplusplus"), "202002L");
    assert_eq!(with_flags(vec![], Some("c++14"), "__cplusplus"), "201402L");
    assert_eq!(with_flags(vec![], Some("c++17"), "__cplusplus"), "201703L");
}

#[test]
fn flag_macros_drive_conditionals() {
    let src = "#ifdef DEBUG\n#if LEVEL > 2\nverbose\n#else\nquiet\n#endif\n#else\nrelease\n#endif";
    assert_eq!(with_flags(vec![d("DEBUG", "1"), d("LEVEL", "3")], None, src), "verbose");
    assert_eq!(with_flags(vec![d("DEBUG", "1"), d("LEVEL", "1")], None, src), "quiet");
    assert_eq!(with_flags(vec![], None, src), "release");
}

#[test]
fn profile_macros() {
    let fs = MemFs::new();
    let src = "#ifdef _MSC_VER\nmsvc\n#endif\n#ifdef __clang__\nclang\n#endif\n#ifdef __GNUC__\ngcc\n#endif";
    let probe = |p: Profile| {
        let mut pp = Preprocessor::new(&fs, PpConfig { profile: p, ..cfg() });
        pp.set_flags(&flags(&[]));
        let mut out = PpOutput::default();
        pp.run(MAIN, src, &mut out);
        norm(&out.render(src))
    };
    assert_eq!(probe(Profile::None), "");
    assert!(probe(Profile::Msvc).contains("msvc"));
    assert!(probe(Profile::Gcc).contains("gcc"));
    let cl = probe(Profile::Clang);
    assert!(cl.contains("clang") && cl.contains("gcc"));
    assert!(probe(Profile::ClangCl).contains("clang"));
}

#[test]
fn force_includes_run_first() {
    let fs = fs_with(&[("pre.h", "#define PRE 5\n")]);
    let mut f = flags(&["/t"]);
    f.force_includes.push("/t/pre.h".into());
    let mut pp = Preprocessor::new(&fs, cfg());
    pp.set_flags(&f);
    let mut out = PpOutput::default();
    pp.run(MAIN, "PRE", &mut out);
    assert_eq!(norm(&out.render("PRE")), "5");
}

// ---- robustness / DoS

#[test]
fn billion_laughs_is_bounded() {
    let mut src = String::from("#define L0 x x\n");
    for i in 1..40 {
        src.push_str(&format!("#define L{i} L{} L{}\n", i - 1, i - 1));
    }
    src.push_str("L39\n");
    let fs = MemFs::new();
    let mut pp = Preprocessor::new(
        &fs,
        PpConfig { max_expansion_tokens: 100_000, ..cfg() },
    );
    pp.set_flags(&flags(&[]));
    let mut out = PpOutput::default();
    pp.run(MAIN, &src, &mut out);
    assert!(out.has_diag(DiagKind::ExpansionLimit));
    assert!(out.tokens.len() < 2_000_000);
}

#[test]
fn deep_parens_in_if_do_not_overflow() {
    let n = 100_000;
    let src = format!("#if {}1{}\nyes\n#else\nno\n#endif\n", "(".repeat(n), ")".repeat(n));
    let (text, out) = run(&src);
    assert_eq!(text, "no");
    assert!(out.has_diag(DiagKind::BadExpression));
    let src = format!("#if {}1\nyes\n#else\nno\n#endif\n", "!".repeat(n));
    let _ = run(&src);
    let src = format!("#if {}1 {}\n#endif\n", "1 ? ".repeat(n), ": 2 ".repeat(n));
    let _ = run(&src);
}

#[test]
fn deeply_nested_macro_calls_do_not_overflow() {
    let n = 50_000;
    let src = format!("#define F(x) x\n{}1{}\n", "F(".repeat(n), ")".repeat(n));
    let _ = run(&src);
}

#[test]
fn deeply_nested_conditionals() {
    let n = 5_000;
    let src = format!("{}x\n{}", "#if 1\n".repeat(n), "#endif\n".repeat(n));
    assert_eq!(run(&src).0, "x");
    let src = format!("{}x\n{}y", "#if 0\n".repeat(n), "#endif\n".repeat(n));
    assert_eq!(run(&src).0, "y");
}

#[test]
fn long_macro_chain_is_linear() {
    let n = 20_000;
    let mut src = String::new();
    for i in 0..n {
        src.push_str(&format!("#define M{i} M{}\n", i + 1));
    }
    src.push_str(&format!("#define M{n} done\nM0\n"));
    assert_eq!(run(&src).0, "done");
}

#[test]
fn huge_argument_list() {
    let n = 100_000;
    let args = (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let src = format!("#define F(...) __VA_ARGS__\nF({args})\n");
    let (text, _) = run(&src);
    assert_eq!(text, norm(&args));
}

#[test]
fn empty_and_whitespace_inputs() {
    assert_eq!(pp1(""), "");
    assert_eq!(pp1("\n\n  \t\n"), "");
    assert_eq!(pp1("#"), "");
    assert_eq!(pp1("#define"), "");
    assert_eq!(pp1("#include"), "");
    assert_eq!(pp1("#if"), "");
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() as usize) % xs.len()]
    }
}

const FRAGMENTS: &[&str] = &[
    "#define ", "#undef ", "#if ", "#ifdef ", "#ifndef ", "#elif ", "#else", "#endif", "#include ",
    "#include \"", "#pragma ", "#error ", "#line ", "#", "##", " # ", "(", ")", ",", "...", "__VA_ARGS__",
    "__VA_OPT__(", "defined", "defined(", "A", "B", "F", "G(", "F(", "x", "1", "0", "0x", "'", "\"",
    "R\"(", ")\"", "/*", "*/", "//", "\\\n", "\n", "\n", "\n", " ", " ", "+", "-", "?", ":", "&&", "||",
    "__LINE__", "__COUNTER__", "__FILE__", "<", ">", "<h.h>", "\"h.h\"", "{", "}", ";", "_Pragma(",
];

#[test]
fn fuzz_fragment_soup_never_panics_or_hangs() {
    let fs = fs_with(&[("h.h", "#define H 1\n#ifdef A\n#endif\n")]);
    let mut pp = Preprocessor::new(&fs, PpConfig { max_expansion_tokens: 1 << 16, ..cfg() });
    pp.set_flags(&flags(&["/t"]));
    let mut out = PpOutput::default();
    let mut rng = Lcg(0xC0FFEE);
    for _ in 0..4000 {
        let mut src = String::new();
        let n = 1 + rng.next() % 60;
        for _ in 0..n {
            src.push_str(rng.pick(FRAGMENTS));
        }
        pp.run(MAIN, &src, &mut out);
        for t in &out.tokens {
            assert!(t.loc as usize <= src.len(), "loc out of range for {src:?}");
            let _ = out.text(t, &src);
        }
        let _ = out.render(&src);
    }
}

// ---- isolation and parallel determinism

#[test]
fn state_does_not_leak_between_runs() {
    let fs = fs_with(&[("h.h", "#define H 1\n")]);
    let fl = flags(&["/t"]);
    let a = "#define LEAK 1\n#include \"h.h\"\n#if 1\n#pragma once\nLEAK H __COUNTER__";
    let b = "LEAK H __COUNTER__ __LINE__\n#ifdef LEAK\nbad\n#endif\n#include \"h.h\"\nH";

    let mut shared = Preprocessor::new(&fs, cfg());
    shared.set_flags(&fl);
    let mut o = PpOutput::default();
    shared.run(MAIN, a, &mut o);
    shared.run(MAIN, b, &mut o);
    let got = norm(&o.render(b));

    let mut fresh = Preprocessor::new(&fs, cfg());
    fresh.set_flags(&fl);
    let mut o2 = PpOutput::default();
    fresh.run(MAIN, b, &mut o2);
    assert_eq!(got, norm(&o2.render(b)));
    assert!(!got.contains("bad"));
}

#[test]
fn switching_flag_sets_switches_macros() {
    let fs = MemFs::new();
    let mut pp = Preprocessor::new(&fs, cfg());
    let mut f1 = flags(&[]);
    f1.macros.push(d("MODE", "1"));
    let mut f2 = flags(&[]);
    f2.macros.push(d("MODE", "2"));
    let mut o = PpOutput::default();
    for _ in 0..3 {
        pp.set_flags(&f1);
        pp.run(MAIN, "MODE", &mut o);
        assert_eq!(norm(&o.render("MODE")), "1");
        pp.set_flags(&f2);
        pp.run(MAIN, "MODE", &mut o);
        assert_eq!(norm(&o.render("MODE")), "2");
    }
}

#[test]
fn parallel_equals_serial() {
    let mut fs = MemFs::new();
    for i in 0..50 {
        fs.add(
            &format!("/t/h{i}.h"),
            &format!("#pragma once\n#define H{i} {i}\n#if H{i} % 2\n#define ODD{i}\n#endif\n"),
        );
    }
    let srcs: Vec<String> = (0..400)
        .map(|i| {
            let mut s = String::new();
            for k in 0..(i % 7 + 1) {
                s.push_str(&format!("#include \"h{}.h\"\n", (i * 3 + k) % 50));
            }
            s.push_str(&format!("#define LOCAL {i}\nint v{i} = LOCAL + __COUNTER__ + __LINE__;\n"));
            s.push_str("#ifdef ODD3\nodd3\n#endif\n");
            s
        })
        .collect();
    let fl = flags(&["/t"]);

    let mut pp = Preprocessor::new(&fs, cfg());
    pp.set_flags(&fl);
    let mut o = PpOutput::default();
    let serial: Vec<String> = srcs
        .iter()
        .map(|s| {
            pp.run(MAIN, s, &mut o);
            norm(&o.render(s))
        })
        .collect();

    let par: Vec<String> = srcs
        .par_iter()
        .map_init(
            || {
                let mut pp = Preprocessor::new(&fs, cfg());
                pp.set_flags(&fl);
                (pp, PpOutput::default())
            },
            |(pp, o), s| {
                pp.run(MAIN, s, o);
                norm(&o.render(s))
            },
        )
        .collect();
    assert_eq!(serial, par);
}

// ---------------------------------------------------------------- random differential fuzz

/// Generates programs with acyclic macros and valid arity, so the standard
/// result is unambiguous and clang is a valid oracle.
fn gen_program(rng: &mut Lcg) -> String {
    let mut s = String::new();
    let nm = 3 + rng.next() % 8;
    let mut arity: Vec<usize> = Vec::new();
    for i in 0..nm {
        let ar = (rng.next() % 4) as usize; // 0 = object-like
        arity.push(ar);
        let params: Vec<String> = (0..ar).map(|p| format!("p{p}")).collect();
        let mut body = Vec::new();
        for _ in 0..(1 + rng.next() % 6) {
            match rng.next() % 7 {
                0 if ar > 0 => body.push(format!("p{}", rng.next() as usize % ar)),
                1 if ar > 0 => body.push(format!("#p{}", rng.next() as usize % ar)),
                2 if ar > 1 => {
                    body.push(format!("p{}##p{}", rng.next() as usize % ar, rng.next() as usize % ar))
                }
                3 if i > 0 => {
                    let j = (rng.next() % i) as usize;
                    if arity[j] == 0 {
                        body.push(format!("M{j}"));
                    } else {
                        let args: Vec<String> = (0..arity[j])
                            .map(|a| if ar > 0 && a % 2 == 0 { "p0".to_string() } else { "k".to_string() })
                            .collect();
                        body.push(format!("M{j}({})", args.join(",")));
                    }
                }
                4 => body.push(format!("t{}", rng.next() % 5)),
                5 => body.push(rng.pick(&["+", "-", "*", ",", "(", ")"]).to_string()),
                _ => body.push(format!("{}", rng.next() % 100)),
            }
        }
        // Keep parens balanced to avoid pathological arg collection mismatches.
        let b = body.join(" ");
        let (o, c) = (b.matches('(').count(), b.matches(')').count());
        let b = format!("{}{b}{}", "( ".repeat(c.saturating_sub(o)), " )".repeat(o.saturating_sub(c)));
        if ar == 0 {
            s.push_str(&format!("#define M{i} {b}\n"));
        } else {
            s.push_str(&format!("#define M{i}({}) {b}\n", params.join(",")));
        }
    }
    for _ in 0..8 {
        let j = (rng.next() as usize) % arity.len();
        if arity[j] == 0 {
            s.push_str(&format!("M{j}\n"));
        } else {
            let args: Vec<String> = (0..arity[j])
                .map(|_| match rng.next() % 4 {
                    0 => String::new(),
                    1 => format!("t{}", rng.next() % 5),
                    2 => format!("{}", rng.next() % 9),
                    _ => {
                        let k = (rng.next() as usize) % arity.len();
                        if arity[k] == 0 { format!("M{k}") } else { format!("x{k}") }
                    }
                })
                .collect();
            s.push_str(&format!("M{j}({})\n", args.join(",")));
        }
    }
    s
}

#[test]
fn random_programs_match_clang() {
    let Some(exe) = clang_exe() else {
        eprintln!("clang not found; skipping random differential test");
        return;
    };
    let mut rng = Lcg(0xFEED_FACE);
    let mut fails = Vec::new();
    let mut compared = 0;
    for i in 0..150 {
        let src = gen_program(&mut rng);
        let Some(want) = clang_pp(&exe, &[], &src, &format!("r{i}")) else { continue };
        let (got, _) = run(&src);
        compared += 1;
        if got != norm(&want) {
            fails.push(format!("--- program {i}\n{src}\nours:  {got}\nclang: {}", norm(&want)));
            if fails.len() >= 5 {
                break;
            }
        }
    }
    assert!(compared > 50, "only {compared} programs compared");
    assert!(fails.is_empty(), "{} mismatches:\n{}", fails.len(), fails.join("\n"));
}
