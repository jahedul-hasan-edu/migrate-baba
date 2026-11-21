# Architecture

## 1. Technology choices

| Concern | Choice | Why |
|---|---|---|
| Language | Rust (stable, edition 2021) | Memory-safe, zero-cost async, single static binary |
| UI | **egui / eframe** (immediate mode, GPU via glow/OpenGL) | Pure Rust, ~10 MB binary, < 300 ms start, 60 fps, trivial virtual scrolling for 100k-row lists, same code on Windows/macOS/Linux, no WebView/Node runtime. A "standard" desktop look is achieved with a restrained theme (system font sizes, 8-px grid, neutral palette). |
| Async runtime | tokio (multi-thread) | Mature; used by tokio-postgres |
| Postgres driver | tokio-postgres | Pure Rust, pipelining, COPY streams |
| TLS | native-tls (SChannel on Windows, Secure Transport on macOS) via postgres-native-tls | OS trust store, no C toolchain needed |
| Secrets | `keyring` crate | Windows Credential Manager / macOS Keychain |
| Diff text | `similar` | Fast Myers/patience line diff for DDL view |
| Config | serde + JSON in OS config dir (`directories`) | |
| CLI | clap | |

Alternative considered: **Tauri + web UI**. Richer widget ecosystem, but requires Node tooling, a WebView
and an IPC hop for every UI update; serialising large diff/progress payloads over IPC costs latency.
egui renders state directly from Rust memory, so the UI can show 100 k objects or live progress of 64
workers with zero serialisation. The core crate is UI-agnostic, so a Tauri front-end can be added later.

## 2. Workspace layout

```
migrate-baba/
├─ Cargo.toml                 (workspace)
├─ crates/
│  ├─ mb-core/                (library — no UI code)
│  │  ├─ conn.rs              connection profiles, TLS, session setup, URL parsing
│  │  ├─ model.rs             Catalog: schemas, extensions, types, sequences, tables, views, functions…
│  │  ├─ introspect.rs        pipelined catalog queries → Catalog
│  │  ├─ ident.rs             identifier/literal quoting
│  │  ├─ ddl.rs               CREATE statements per object (also used for side-by-side view)
│  │  ├─ diff.rs              object-level comparison → ObjectDiff list (status + details)
│  │  ├─ plan.rs              desired-vs-current → ordered Steps (SQL + hazards)
│  │  ├─ deploy.rs            test-deploy / deploy executors
│  │  ├─ data/
│  │  │  ├─ copy.rs           parallel COPY engine (snapshot, ctid split, progress)
│  │  │  ├─ compare.rs        chunked hash compare + row drill-down
│  │  │  └─ sync.rs           staging-table upsert/delete sync
│  │  └─ progress.rs          progress/event types, cancellation
│  ├─ mb-app/                 (binary `migrate-baba` — egui desktop app)
│  │  ├─ main.rs, app.rs      app shell, event loop bridge
│  │  ├─ store.rs             profiles/settings/history persistence + keychain
│  │  ├─ theme.rs, widgets.rs, sql_highlight.rs
│  │  └─ views/{connections,schema_compare,data_compare,migrate,history,settings}.rs
│  └─ mb-cli/                 (binary `mbaba`)
└─ docs/specs/
```

## 3. Threading model

```
 ┌───────────── UI thread (eframe) ─────────────┐        ┌──────── tokio runtime (N threads) ────────┐
 │ update() every frame:                         │ spawn  │ introspect(source) ─┐                      │
 │   1. drain Event channel (non-blocking)       │ ─────► │ introspect(target) ─┴─► diff → plan        │
 │   2. mutate AppState                          │        │ copy workers (k pairs of connections)      │
 │   3. render from AppState (no I/O ever)       │ ◄───── │ send(Event) + ctx.request_repaint()        │
 └───────────────────────────────────────────────┘ events └────────────────────────────────────────────┘
```

* The UI thread **never awaits**. All I/O is spawned on the runtime; results return as `Event`s through an
  unbounded `std::sync::mpsc` channel; each send calls `egui::Context::request_repaint()` so the UI wakes
  immediately (no polling, idle CPU ≈ 0%).
* High-frequency progress (bytes/rows) uses shared `AtomicU64` counters read by the UI each frame instead
  of one event per chunk — O(1) cost regardless of throughput. While a job runs the UI repaints at ≤ 30 Hz.
* Cancellation: `tokio_util::sync::CancellationToken` per job; workers check it between chunks and
  `select!` on it during awaits; DB work rolls back.

## 4. Core data flow — schema compare

```
Profile ─► connect (TLS) ─► SET search_path='' ─► pipelined catalog queries ─► Catalog (source)
                                                                               Catalog (target)
             filter (schemas, ignore options) ◄─────────────────────────────────────┘
                       │
                       ├─► diff::compare(src, tgt) → Vec<ObjectDiff>  (tree, side-by-side DDL)
                       └─► plan::build(src, tgt, selection, opts) → Plan { steps: Vec<Step> }
```

**Selection trick**: to generate a plan for a subset, build a *desired* catalog = target with each selected
object replaced by its source version (or removed if absent in source). Then plan(target → desired).
Unselected objects are identical in both, so they produce no steps, while dependency handling (e.g.
dropping a view that depends on an altered column) still works because the full catalog is present.

## 5. Error model
* `mb_core::Error` (thiserror): `Connect`, `Tls`, `Query{sql, source}`, `Cancelled`, `Unsupported`, `Io`.
* Every DB error surfaced to the UI includes SQLSTATE, message, detail, hint and the statement.
* Workers never panic across the runtime boundary; failures become `Event::JobFailed`.

## 6. Persistence
* `<config_dir>/MigrateBaba/profiles.json` — profiles (no secrets).
* `<config_dir>/MigrateBaba/settings.json` — settings.
* `<data_dir>/MigrateBaba/history/<timestamp>-<kind>.json` (+ `.sql`) — run history.
* Keychain service `migrate-baba`, account = profile id.

## 7. Extensibility
* `Catalog` and `Plan` are engine-agnostic structs; a `Dialect` trait can be introduced when a second engine
  is added.
* Supabase Management API client (Edge Functions) lives in `mb-core::supabase` behind a feature flag.
