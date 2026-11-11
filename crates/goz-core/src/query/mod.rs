//! The query engine: case folding, the query grammar, and the executor that
//! scans a [`crate::index::VolumeIndex`] and returns a sorted page of hits.
//!
//! [`fold`] provides case folding; [`parse`] reads a query string (with
//! the `lex` tokens) into a [`parse::ParsedQuery`] and its boolean
//! expression (`expr`), with [`dates`] resolving each `dm:` value and `glob`
//! compiling each wildcard; [`engine`] compiles it for a volume (the
//! expression in `cexpr`) and runs it against the index with scope, sort,
//! and pagination, within a [`budget::Budget`] of time.

pub mod budget;
mod cexpr;
pub mod content;
pub mod dates;
pub mod engine;
mod expr;
mod folder_rank;
mod glob;
mod lex;
pub mod parse;
#[cfg(test)]
mod parse_v060;
mod pattern;
mod related;
mod text;
mod value;

/// Case folding lives at the crate root (`crate::fold`) so the index can build
/// its folded shadow arena without depending on the query module; re-exported
/// here for callers that reach for `query::fold`.
pub use crate::fold;

pub use budget::{Budget, Stopped};
pub use content::{
    ContentEncoding, ContentEnv, ContentQuery, ContentTerm, MAX_CONTENT_TERMS, TextFilter,
    sort_items,
};
pub use dates::{DateFilter, DateOrder, LocalClock};
pub use engine::{
    CompiledQuery, HitRef, QueryHit, QueryOutcome, RefOutcome, extension_key, mtime_key,
    resolve_scope, run_query, run_query_refs, size_key, sort_extension,
};
pub use expr::{Anchors, Expr, FolderPath, Leaf, LenOf, Skip, Target};
pub use parse::{
    Filters, Kind, ParsedQuery, QueryError, SizeRange, Wildcard, parse_query, parse_query_at,
    parse_query_with_case,
};
pub use pattern::RegexTerm;
