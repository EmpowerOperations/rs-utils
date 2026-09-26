# empower-rs-utils

Shared utilities for EmpowerOps Rust projects. Not a framework, not a platform — a small set of
things that were already written twice and are cheaper to maintain once.

Consumed by **artemis** today. **surro** next, then two further projects that need Rust FFI.

## What's in it

Two feature-gated modules, so a consumer takes only what it uses:

| module | feature | what |
|---|---|---|
| `report` | `report` *(default)* | Semicolon-CSV tables: fixed-width, Excel-friendly, upsert or append. |
| `ffi` | `ffi` | `HandleStore`, `AbiThreadMarshaller<T>`, `PointerRegistry` — state behind a C boundary. |

```toml
# a benchmark harness, no FFI machinery linked
empower-rs-utils = { git = "https://github.com/EmpowerOperations/rs-utils", rev = "..." }

# an FFI crate, no CSV machinery linked
empower-rs-utils = { git = "https://github.com/EmpowerOperations/rs-utils", rev = "...", default-features = false, features = ["ffi"] }

# workspace root, to build against a local submodule checkout instead
[patch."https://github.com/EmpowerOperations/rs-utils"]
empower-rs-utils = { path = "utils" }
```

### Biggest by volume: the reporting

Most of the code here today is benchmark-reporting machinery — the semicolon-CSV format, the
magnitude-bucketed float formatter, the upsert-or-append reconciliation. It arrived first because
it was the most obviously duplicated: `format_float` had independently grown **byte-identical** in
surro and artemis, which is about as clear a signal as de-duplication ever gives you.

### Probably most valuable: the `ffi` module

The reporting saves typing. The `ffi` module saves a class of bug that is genuinely hard to find.

Two separate jobs, composed by the consumer:

* **`HandleStore`** turns integer handles a C caller holds into owned Rust values. `Handle<T>` is
  a tagged `u64` that stays `Copy`/`Send` even when `T` is neither. Pure data, no threading.
* **`AbiThreadMarshaller<T>`** keeps a `T` on one thread, where Rust's `Send`/`Sync` checking
  cannot help: the boundary reconstructs a context pointer independently on every call, so nothing
  stops a C caller handing it to a different thread. `ThreadStrategy::Direct` catches that at
  runtime via a thread fingerprint; `ThreadStrategy::Marshalled` sidesteps it by giving `T` a
  worker thread of its own. `T` is built by an init closure *on* its owning thread, so it need not
  be `Send`.

A consumer's per-context state is typically `struct State { handles: HandleStore, ... }` with
anything else it needs (a licensor, say) alongside, owned by an `AbiThreadMarshaller<State>`.

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
* Worse: `Handle<T>` crosses the C boundary as a raw `u64`. Inside Rust the type
  system keeps two copies' keys apart; **across FFI the type is erased**, so a key minted by one
  copy and consumed by the other compiles cleanly and misbehaves at runtime — precisely where the
  type safety was supposed to be doing the work.

So every consumer depends on this crate by its **git URL**, never by `path`, and a workspace that
wants a local checkout patches that URL at its root (above). A `path` dependency inside a git
dependency belongs to that git repository's source, so no patch from outside can reach it. Check
with `cargo tree -i empower-rs-utils`: exactly one entry.

## Building

Standalone workspace, so it builds on its own as a submodule:

```bash
cargo test --features ffi     # all modules
cargo test                    # report only
```
