# empower-rs-utils

Shared utilities for EmpowerOps Rust projects. Not a framework, not a platform — a small set of
things that were already written twice and are cheaper to maintain once.

Consumed by **artemis** today. **surro** next, then two further projects that need Rust FFI.

## What's in it

Two feature-gated modules, so a consumer takes only what it uses:

| module | feature | what |
|---|---|---|
| `report` | `report` *(default)* | Semicolon-CSV tables: fixed-width, Excel-friendly, upsert or append. |
| `ffi_context` | `ffi` | `LockingContext` — thread-identity-checked state for a C boundary. |

```toml
# a benchmark harness, no FFI machinery linked
empower-rs-utils = { path = "../utils" }

# an FFI crate, no CSV machinery linked
empower-rs-utils = { path = "../utils", default-features = false, features = ["ffi"] }
```

### Biggest by volume: the reporting

Most of the code here today is benchmark-reporting machinery — the semicolon-CSV format, the
magnitude-bucketed float formatter, the upsert-or-append reconciliation. It arrived first because
it was the most obviously duplicated: `format_float` had independently grown **byte-identical** in
surro and artemis, which is about as clear a signal as de-duplication ever gives you.

### Probably most valuable: `LockingContext`

The reporting saves typing. `LockingContext` saves a class of bug that is genuinely hard to find.

It gates access to state that crosses a C ABI, where Rust's `Send`/`Sync` checking cannot help:
the boundary reconstructs a context pointer independently on every call, so nothing stops a
misbehaving C caller handing it to a different thread. `Direct` catches that at runtime via a
thread fingerprint; `Marshaled` sidesteps it by giving the state its own worker thread. Handles
into its typed store are `LockingContextKey<T>` — a tagged `u64` that stays `Copy`/`Send` even
when `T` is neither.

Four projects will each need this, and each would otherwise write a subtly different version.

## Using the report module

Every function takes the file it should act on. **The crate resolves nothing** — no environment
variables, no derived paths. Where output belongs is the consuming project's business.

```rust
use empower_rs_utils::report::{self, NameWidthPair, RowMap, format_float};

const COLUMNS: &[NameWidthPair] = &[("version", 24), ("mean", 12), ("run-count", 10)];

let row: RowMap = [
    ("version", env!("CARGO_PKG_VERSION").to_string()),
    ("mean", format_float(mean)),
    ("run-count", n.to_string()),
].into_iter().collect();

// Show the previous row beside this one before writing it.
report::print_comparison(&mut io::stderr(), &path, COLUMNS, &row)?;

// Curated: one row per version. Re-running a version replaces it.
report::upsert_rows(&path, COLUMNS, "version", env!("CARGO_PKG_VERSION"), &[row.clone()])?;

// Audit: complete history, including superseded runs.
report::append_row(&audit_path, COLUMNS, &row)?;
```

Rows are keyed by column **name**, so a missing value yields an empty cell instead of shifting
every column after it — the failure mode of parallel header/value arrays.

`upsert_rows` replaces the trailing **block** of rows sharing the key, not just the last row. A
summary file holds one row per version; a per-run sidecar holds ten. Both need replacing whole.

## ⚠ If more than one dependency path reaches this crate

Cargo unifies git dependencies by URL, but **`path` dependencies from different directories are
different sources and do not unify.** Once surro also depends on this crate, artemis reaches it
twice — directly, and through surro — and would compile **two separate copies**.

That is not cosmetic:

* `CSV_WRITE_LOCK` is a crate-level `static`. Two copies, two mutexes, no mutual exclusion.
* Worse: `LockingContextKey<T>` crosses the C boundary as a raw `u64`. Inside Rust the type
  system keeps two copies' keys apart; **across FFI the type is erased**, so a key minted by one
  copy and consumed by the other compiles cleanly and misbehaves at runtime — precisely where the
  type safety was supposed to be doing the work.

**Before a second consumer adopts this crate**, make both resolve to one copy: a workspace
`[patch]`, or have both reference the same git URL. See `AGENTS.md`.

## Building

Standalone workspace, so it builds on its own as a submodule:

```bash
cargo test --features ffi     # all modules
cargo test                    # report only
```
