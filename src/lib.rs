//! Lightspeed: a database-backed, speed-first C++ indexer and lightweight LSP.
//!
//! Modules, from the bottom up (each depends only on those above it):
//! * [`base`] — typed ids, flat multimaps, string arena, hashing, mmap file loading
//! * [`lexer`] — zero-allocation tokenizer
//! * [`pp`] — tolerant preprocessor (conditionals, macros, includes)
//! * [`symbols`] — flat symbol database and side indexes
//! * [`parser`], [`scope_decls`] — declaration scanner with opaque function bodies
//! * [`hierarchy`] — inheritance DAG and virtual function families
//! * [`store`] — on-disk cache of per-file analysis
//! * [`workspace`] — compile database, dependency graph and the project [`workspace::index::Index`]
//! * [`query`] — definition, references, hover, completion, semantic tokens...
//! * [`lsp`] — the JSON-RPC language server
//! * [`testgen`] — synthetic C++ for tests and benchmarks

pub mod base;
pub mod hierarchy;
pub mod lexer;
pub mod lsp;
pub mod parser;
pub mod pp;
pub mod query;
pub(crate) mod scope_decls;
pub mod store;
pub mod symbols;
#[doc(hidden)]
pub mod testgen;
pub mod workspace;

pub use base::source::{LineIndex, LoadError, LoadOptions, SourceFile};
pub use lexer::{Keyword, Lexer, Token, TokenFlags, TokenKind, lex_into};
pub use symbols::{FileId, NameId, SymbolDb, SymbolId};
