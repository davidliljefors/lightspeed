//! The project as a whole: the compile database, flag and include resolution, the
//! include dependency graph, and the [`index::Index`] that ties every file's
//! symbols into one queryable database.

pub mod compile_commands;
pub mod depgraph;
pub mod index;
pub(crate) mod sysinc;
