//! Lightspeed: a database-backed, speed-first C++ indexer and lightweight LSP.
//!
//! Module map (phase in parentheses):
//! * [`ids`], [`csr`], [`interner`] — typed IDs, flat multimaps, string arena (4)
//! * [`source`] — mmap/owned zero-copy file loading, line index (2)
//! * [`lexer`] — zero-allocation tokenizer (2)
//! * [`symbols`] — flat symbol database and side indexes (4)
//! * [`testgen`] — synthetic C++ for tests and benchmarks
//!
//! Planned: `compile_commands` + `cache` (1), `preprocessor` (2), `parser` +
//! opaque blocks (3), `inheritance` + virtual families (4), `watcher` + `lsp` (5).

pub mod cache;
pub mod compile_commands;
pub mod csr;
pub mod depgraph;
pub mod hash;
pub mod ids;
pub mod interner;
pub mod lexer;
pub mod mem;
pub mod pp;
pub mod source;
pub mod symbols;
#[doc(hidden)]
pub mod testgen;

pub use lexer::{Keyword, Lexer, Token, TokenFlags, TokenKind, lex_into};
pub use source::{LineIndex, LoadError, LoadOptions, SourceFile};
pub use symbols::{FileId, NameId, SymbolDb, SymbolId};
pub mod parser;
pub mod scope_decls;
pub mod hierarchy;
pub mod index;
pub mod query;
pub mod lsp;
pub mod sysinc;
