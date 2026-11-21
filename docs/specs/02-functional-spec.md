# Functional Specification

Requirement IDs: `FR-<area>-<n>`. Priority: **P0** = v1 must, **P1** = v1 should, **P2** = later.

## 1. Connections (FR-CON)

| ID | Pri | Requirement |
|----|-----|-------------|
| FR-CON-1 | P0 | Create/edit/delete/duplicate saved connection profiles: name, provider, host, port, database, user, password, SSL mode, colour tag. |
| FR-CON-2 | P0 | Paste a connection URL (`postgres://` / `postgresql://`, incl. `?sslmode=`) to fill the form. |
| FR-CON-3 | P0 | Provider presets: Generic, Supabase (direct + session pooler), Neon, AWS RDS/Aurora, Google Cloud SQL, Azure, DigitalOcean, Render, Railway, Crunchy Bridge, Timescale, Local. Presets set default port, SSL mode and excluded schemas. |
| FR-CON-4 | P0 | SSL modes: `disable`, `prefer`, `require` (encrypt, no verification — libpq semantics), `verify-full` (verify chain + hostname). |
| FR-CON-5 | P0 | "Test connection" shows: server version, round-trip latency (ms), current user, whether the user is superuser / can set `session_replication_role`. |
| FR-CON-6 | P0 | Passwords stored in the OS keychain (Windows Credential Manager / macOS Keychain); profile file never contains secrets. |
| FR-CON-7 | P1 | Warn when a Supabase **transaction pooler** (port 6543) is used: prepared statements & snapshots are unsupported — suggest direct or session pooler. |
| FR-CON-8 | P1 | Environment colour tag (e.g., red = production) shown in the source/target bar. |
| FR-CON-9 | P2 | SSH tunnel; client certificates. |

## 2. Workspace bar
* FR-WS-1 (P0): Persistent top bar with **Source ⟶ Target** selectors, swap button (⇄), and the primary
  action of the current screen (Compare / Copy).
* FR-WS-2 (P0): Schema filter: include/exclude schemas (defaults from provider preset).

## 3. Schema Compare (FR-SC)

### 3.1 Object coverage

| Object | Create | Alter | Drop | Notes |
|---|---|---|---|---|
| Schema | ✔ | comment | ✔ | |
| Extension | ✔ | version update | ✔ | `plpgsql` and provider-managed ignored |
| Enum type | ✔ | `ADD VALUE` (ordered) | ✔ | Label removal/reorder → flagged *manual* |
| Domain | ✔ | default, not null, check constraints | ✔ | base type change → drop+create (hazard) |
| Composite type | ✔ | add/drop/alter attribute | ✔ | |
| Sequence | ✔ | type/increment/min/max/cache/cycle | ✔ | identity sequences managed through column |
| Table | ✔ | see columns etc. | ✔ | unlogged, partitioned, partitions, RLS enabled/forced, comment |
| Column | ✔ | type (USING cast), default, not null, identity, collation, generated expr, comment | ✔ | |
| Constraint (PK/UNIQUE/CHECK/EXCLUDE/FK) | ✔ | drop+add | ✔ | FKs created last, dropped first |
| Index | ✔ | drop+create | ✔ | optional `CONCURRENTLY` |
| Trigger | ✔ | drop+create, enable/disable | ✔ | |
| RLS policy | ✔ | drop+create | ✔ | |
| View | ✔ | drop+create (with dependants) | ✔ | |
| Materialized view | ✔ | drop+create (with dependants) | ✔ | |
| Function / procedure (Supabase RPC) | ✔ | `CREATE OR REPLACE`; drop+create if return type/kind changed | ✔ | |
| Comments | ✔ | ✔ | ✔ | on schema/table/column/view/function/type |
| Privileges (GRANT/REVOKE) | P1 | | | table/function/schema ACLs |
| Publications (Realtime) | P2 | | | |
| Supabase Edge Functions | P1 | via Management API (list, diff by hash, deploy) | | Not stored in Postgres |
| pg_cron jobs, storage buckets | P2 | as data in `cron.job`, `storage.buckets` | | |

### 3.2 Requirements

| ID | Pri | Requirement |
|----|-----|-------------|
| FR-SC-1 | P0 | Introspect source & target **concurrently**; all catalog queries pipelined on one connection per side. |
| FR-SC-2 | P0 | Result tree grouped by status: **Only in source (create)**, **Different (alter)**, **Only in target (drop)**, **Identical** (hidden by default); counts per group. |
| FR-SC-3 | P0 | Filter by object kind, text search, "show identical" toggle. |
| FR-SC-4 | P0 | Checkbox per object, per group, per kind; select all / none. |
| FR-SC-5 | P0 | Selecting an object shows **side-by-side DDL diff** (source left, target right), changed lines highlighted, plus a list of change details ("column `age` type int4 → int8"). |
| FR-SC-6 | P0 | Generate the migration script for the **selected** objects only; dependency-correct order. |
| FR-SC-7 | P0 | Hazards annotated per statement: `DATA_LOSS`, `TABLE_REWRITE`, `MAY_FAIL` (e.g., SET NOT NULL with nulls), `LOCKS_TABLE`, `MANUAL` (cannot auto-generate). |
| FR-SC-8 | P0 | Script actions: copy, save as `.sql`, save as timestamped migration file (`YYYYMMDDHHMMSS_name.sql` — Supabase CLI compatible). |
| FR-SC-9 | P0 | **Test deploy**: execute the script in a transaction on the target and `ROLLBACK`; report first failing statement. |
| FR-SC-10 | P0 | **Deploy**: single transaction (default) or statement-by-statement; non-transactional statements (e.g. `CREATE INDEX CONCURRENTLY`) run after commit. Destructive scripts require typing the target database name. |
| FR-SC-11 | P0 | After deploy, auto re-compare and show remaining differences (should be zero). |
| FR-SC-12 | P1 | Options: ignore comments, ignore owners, ignore whitespace in function bodies, use `CASCADE` on drops, `CONCURRENTLY` indexes, `NOT VALID` FKs + separate `VALIDATE`. |
| FR-SC-13 | P1 | Rename hints: a removed + added column with same type at the same position is offered as `RENAME COLUMN`. |
| FR-SC-14 | P1 | Export a compare report (HTML/Markdown). |

## 4. Data Compare (FR-DC)

| ID | Pri | Requirement |
|----|-----|-------------|
| FR-DC-1 | P0 | Compare tables that exist in both (matched by schema.name); show source rows, target rows, status. |
| FR-DC-2 | P0 | Tables with a PK: chunked comparison — key-range chunks hashed **server-side** (`md5(string_agg(md5(row::text)))`); only hashes cross the wire. |
| FR-DC-3 | P0 | For mismatched chunks, drill down to row level: counts of *only in source*, *only in target*, *different*; sample keys shown. |
| FR-DC-4 | P0 | Tables without a PK: whole-table count + ordered hash; sync only via replace. |
| FR-DC-5 | P0 | Session settings normalised on both sides (`DateStyle`, `TimeZone`, `extra_float_digits`, `bytea_output`, `IntervalStyle`) so text forms hash identically. |
| FR-DC-6 | P0 | "Sync selected": apply inserts/updates/deletes to target for mismatched chunks only. |
| FR-DC-7 | P1 | Column include/exclude per table; custom key for tables without PK. |
| FR-DC-8 | P2 | WHERE filter per table. |

## 5. Data Migration (FR-DM)

| ID | Pri | Requirement |
|----|-----|-------------|
| FR-DM-1 | P0 | Choose tables (tree with checkboxes, est. rows & size). |
| FR-DM-2 | P0 | Modes: **Replace** (truncate selected target tables then copy), **Append** (copy only), **Sync** (upsert + delete via staging; requires PK, falls back to Replace). |
| FR-DM-3 | P0 | Optional "create missing tables first" (runs schema plan for selected tables). |
| FR-DM-4 | P0 | Streaming `COPY … TO STDOUT` → `COPY … FROM STDIN`, **binary** when column types are identical built-ins, text otherwise; no row materialisation in the app. |
| FR-DM-5 | P0 | N parallel workers (default = min(8, CPU cores)); large tables split into ctid ranges (PG14+) processed in parallel. |
| FR-DM-6 | P0 | Consistent snapshot across workers via `pg_export_snapshot()` (falls back with a warning). |
| FR-DM-7 | P0 | Disable triggers/FK checks via `session_replication_role = replica` when permitted; otherwise load in FK-topological waves. |
| FR-DM-8 | P0 | Reset sequences/identity to `max(col)` after load. |
| FR-DM-9 | P0 | Live progress: per-table state, rows, bytes, throughput MB/s, ETA, overall bar; cancel. |
| FR-DM-10 | P0 | Per-table error isolation: one failing table doesn't stop others (option: stop on first error). |
| FR-DM-11 | P1 | Resume: skip tables already completed in the previous run of the same job. |
| FR-DM-12 | P2 | Column mapping / transforms; masking (PII anonymisation). |

## 6. History & logs (FR-HI)
* FR-HI-1 (P0): Every deploy/copy/sync stored with timestamp, source/target names, script, duration, outcome.
* FR-HI-2 (P0): Live log panel with levels; copy log.
* FR-HI-3 (P1): Re-open a history item, re-run script.

## 7. Settings (FR-SET)
* Theme (dark/light/system), UI scale, default workers, statement timeout, default excluded schemas,
  default compare options, chunk size for data compare.

## 8. CLI (FR-CLI)
* `mbaba diff --source URL --target URL [--schema public] [--out plan.sql]`
* `mbaba apply --target URL --file plan.sql [--dry-run]`
* `mbaba copy --source URL --target URL [--tables a,b] [--mode replace|append|sync] [--workers 8]`
* `mbaba data-diff --source URL --target URL [--tables …]`
* Exit codes: 0 = no diff / success, 1 = error, 2 = differences found (for CI drift detection).
