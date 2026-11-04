//! The query engine: case folding, the query grammar, and the executor that
//! scans a [`crate::index::VolumeIndex`] and returns a sorted page of hits.
//!
//! [`fold`] provides case folding; [`parse`] turns a query string into a
//! [`parse::ParsedQuery`] (loudly rejecting unsupported operators), with
//! [`dates`] resolving each `dm:` value; [`engine`] compiles and runs it
//! against the index with scope, sort, and pagination.

pub mod dates;
pub mod engine;
pub mod parse;

/// Case folding lives at the crate root (`crate::fold`) so the index can build
/// its folded shadow arena without depending on the query module; re-exported
/// here for callers that reach for `query::fold`.
pub use crate::fold;

pub use dates::{DateFilter, LocalClock};
pub use engine::{
    CompiledQuery, HitRef, QueryHit, QueryOutcome, RefOutcome, resolve_scope, run_query,
    run_query_refs,
};
pub use parse::{
    Filters, Kind, ParsedQuery, QueryError, SizeRange, Wildcard, parse_query, parse_query_at,
    parse_query_with_case,
};
