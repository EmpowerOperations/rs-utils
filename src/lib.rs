//! Shared utilities for EmpowerOps Rust projects.
//!
//! This crate is a deliberately thin collection of helpers, not a framework. Two rules keep it
//! from drifting into an inner platform:
//!
//! 1. **It resolves nothing.** No environment variables are read, no paths are derived, no
//!    conventions about where output belongs are encoded. Callers pass `&Path` and the crate
//!    writes there. Deciding *where* is the consuming project's business -- typically its own
//!    `build.rs`, which stays that project's private concern.
//! 2. **It speaks in standard types.** `&Path`, `&[(&str, u8)]`, `HashMap<&str, String>`,
//!    `impl Write`. Nothing here knows what a benchmark, a model or an optimizer is; each
//!    consumer keeps its own column definitions and its own row-building code.
//!
//! Where an operation could be a pure function, it is one. Filesystem access is confined to the
//! few functions that exist to perform it.

#[cfg(feature = "report")]
pub mod report;

#[cfg(feature = "ffi")]
pub mod ffi;

/// Force-resolve `$path` (a compile error if it does not exist), then return just the final
/// identifier as a `&'static str`.
///
/// Lets a benchmark name be tied to the function that produces it, so renaming the function
/// cannot silently leave a stale string behind in the output.
#[macro_export]
macro_rules! name_of {
    ($path:path) => {{
        let _ = &$path; // force name resolution; works for functions and statics alike
        let s = stringify!($path);
        s.rsplit("::").next().unwrap()
    }};
}
