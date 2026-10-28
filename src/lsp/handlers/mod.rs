//! One file per feature; each adds `impl Server` methods that the router's table points at.

mod completion;
mod hierarchy;
mod hover;
mod links;
mod navigation;
mod rename;
mod semantic_tokens;
mod symbols;
