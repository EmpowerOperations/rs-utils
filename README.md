# empower-rs-utils

Shared utilities for EmpowerOps Rust projects. Not a framework, not a platform — a small set of
things that were already written twice and are cheaper to maintain once.

Consumed by **artemis** and **hd-surrogates**, with two further projects that need Rust FFI to
follow.

## What's in it

Three modules, always all compiled: the crate is too small for cargo features to pay for
themselves.

| module | what |
|---|---|
| `report` | Semicolon-CSV tables: fixed-width, Excel-friendly, upsert or append. |
| `table` | Delimited tables of numbers, read and written: exact round trips, decimal commas read where the comma doesn't separate cells. |
| `ffi` | `HandleStore`, `AbiThreadMarshaller<T>`, `PointerRegistry` — state behind a C boundary. |

```toml
[dependencies]
empower-rs-utils = "0.1"

# workspace root, to build against a local submodule checkout instead
[patch.crates-io]
empower-rs-utils = { path = "utils" }
```

### Biggest by volume: the reporting

Most of the code here today is benchmark-reporting machinery — the semicolon-CSV format, the
magnitude-bucketed float formatter, the upsert-or-append reconciliation. It arrived first because
it was the most obviously duplicated: `format_float` had independently grown **byte-identical** in
surro and artemis, which is about as clear a signal as de-duplication ever gives you.

### Read anywhere: the `table` module

`report` writes ledgers for people. `table` is for data that moves between programs and
spreadsheets: a header of names, then rows of numbers, comma, semicolon, tab or whitespace
separated. Reading guesses the delimiter and an optional header, and accepts a decimal comma
where the comma isn't the separator, so a sheet exported in a `1,5` locale reads as numbers
rather than splitting each value in two. Writing always uses a decimal point and the shortest
text that parses back to the same `f64`, so a table read back is bit for bit what was written.
The splitting and quoting are the `csv` crate's, the crate's one dependency.

### Probably most valuable: the `ffi` module

The reporting saves typing. The `ffi` module saves a class of bug that is genuinely hard to find.

Two separate jobs, composed by the consumer:

* **`HandleStore`** turns integer handles a C caller holds into owned Rust values. `Handle<T>` is
  a tagged `u64` that stays `Copy`/`Send` even when `T` is neither. Pure data, no threading.
  `SendHandleStore` is the same for `Send` values only, and so is `Send` itself.
* **`AbiThreadMarshaller<T>`** keeps a `T` on one thread, where Rust's `Send`/`Sync` checking
  cannot help: the boundary reconstructs a context pointer independently on every call, so nothing
  stops a C caller handing it to a different thread. `ThreadStrategy::Direct` catches that at
  runtime via a thread fingerprint; `ThreadStrategy::Marshalled` sidesteps it by pinning `T` to one
  worker of a process-wide pool, which builds it, runs every call on it and drops it. `T` is built
  by an init closure *on* its owning thread, so it need not be `Send`.

  The pool has one worker per CPU, or `EMPOWEROPS_RS_UTILS_THREAD_POOL_SIZE` of them; a worker
  starts with its first context and exits with its last, so threads stay bounded however many
  contexts are alive. Contexts sharing a worker take turns, so a marshalled closure must never wait
  on another marshalled call, which may be queued behind it: doing so from inside one panics.

* **`SharedSequential<T: Send>`** is the simpler answer when the state is `Send`: a mutex, so any
  thread may call it, one call at a time, and drop it. The `Send` bound is checked where the state
  is wrapped, so the compiler, not a runtime check, keeps `!Send` values out.

A consumer's per-context state is typically `struct State { handles: HandleStore, ... }` with
anything else it needs (a licensor, say) alongside, owned by an `AbiThreadMarshaller<State>`; or,
when all of it is `Send`, `struct State { handles: SendHandleStore, ... }` in a
`SharedSequential<State>`.

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

Cargo unifies a dependency only when every path to it names the **same source and a
semver-compatible version**. Once surro also depends on this crate, artemis reaches it twice —
directly, and through surro — and compiles **two separate copies** if those two paths disagree:
`0.1` against `0.2`, crates.io against git, or a `path` from two different directories.

That is not cosmetic:

* `CSV_WRITE_LOCK` is a crate-level `static`. Two copies, two mutexes, no mutual exclusion.
* Worse: `Handle<T>` crosses the C boundary as a raw `u64`. Inside Rust the type
  system keeps two copies' keys apart; **across FFI the type is erased**, so a key minted by one
  copy and consumed by the other compiles cleanly and misbehaves at runtime — precisely where the
  type safety was supposed to be doing the work.

So every consumer depends on this crate **from crates.io**, never by `path` or git URL, and keeps
to one semver-compatible version across the tree. A workspace that wants a local checkout patches
crates.io at its root (above), which reaches transitive dependents too. Check with
`cargo tree -i empower-rs-utils`: exactly one entry.

## Building

Standalone workspace, so it builds on its own as a submodule:

```bash
cargo test
```

## License

Apache-2.0; see `LICENSE`.
