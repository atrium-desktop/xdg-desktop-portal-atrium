//! Filesystem helpers with a narrow, named purpose.
//!
//! ADR-0025 dissolved the former `files` grab bag into modules that say what
//! they hold. Each submodule has one job:
//!
//! - [`atomic`]: create-temp-then-rename writes under an owner-verified
//!   directory.
//!
//! `file://` URI handling lives beside it as [`crate::uri`]: it is about names,
//! not about writing files, and its callers are the ones that build portal
//! result dictionaries.

pub(crate) mod atomic;
