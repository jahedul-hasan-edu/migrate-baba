# Competitive Analysis — what the best tools do, and what we take

Survey of the leading paid and free schema/data migration tools for PostgreSQL. For each: the ideas worth
adopting (✅) and the pain points we design against (❌).

## 1. Paid / commercial

### Redgate SQL Compare + Data Compare (gold standard UX, SQL Server first, Postgres via Flyway Desktop)
* ✅ Two-pane **compare → select → review script → deploy** workflow. Results grouped as
  *Only in source / Different / Only in target / Identical*, with per-object checkboxes.
* ✅ Side-by-side DDL with synchronized scrolling and changed lines highlighted.
* ✅ "Deployment warnings" (data loss, table rebuild) shown before deployment.
* ✅ Data Compare: per-table counts of *different / only-in-source / only-in-target / identical* rows,
  drill-down grid, generate sync script.
* ✅ Saved comparison projects; command line for CI.
* ❌ Windows-only, expensive, Postgres support is second-class.

### Navicat Premium (Structure Synchronization, Data Synchronization, Data Transfer)
* ✅ Wizard style: pick source/target → compare → deploy. Very approachable.
* ✅ Data transfer across servers with options: create tables, truncate, continue on error.
* ❌ Coarse dependency handling; RLS/policies & functions often wrong; slow on large data.

### JetBrains DataGrip / DBeaver PRO (Schema Compare via Liquibase)
* ✅ Integrated into an IDE users already know; diff viewer is excellent (DataGrip).
* ❌ JVM start-up and memory; Liquibase diff misses many Postgres features (policies, functions bodies).

### Bytebase / Atlas Cloud / Liquibase Pro / Flyway Enterprise
* ✅ **Lint & risk analysis** of migration SQL (e.g., "adding NOT NULL without default locks table").
* ✅ Change history / audit, approvals, drift detection.
* ✅ Atlas: declarative "desired state" model → plan; clear ordering phases.
* ❌ Server products / workflow-heavy for a single developer.

### AWS DMS / Google DMS / Azure DMS
* ✅ Full load + CDC, table mappings, validation (row counts & hashes).
* ❌ Cloud-locked, slow to set up, opaque failures, cost.

## 2. Free / open source

### migra (djrobstep) — and Supabase CLI `db diff` (migra / pg-delta / pg-schema-diff)
* ✅ Pure catalog-based diff, outputs *the SQL to make target look like source*. Handles functions,
  policies, enums, extensions, privileges. **Our core model follows this approach.**
* ✅ Supabase-aware exclusion of managed schemas (`auth`, `storage`, `realtime` …).
* ❌ CLI only, Python start-up, unmaintained; no data features.

### Stripe pg-schema-diff
* ✅ Online-safe plans: `CREATE INDEX CONCURRENTLY`, `NOT VALID` + `VALIDATE` constraints,
  hazards annotation (`HAZARD: deletes data`, `HAZARD: acquires access exclusive lock`).
  **We adopt hazard annotations and "online-safe" options.**

### pgAdmin 4 Schema Diff
* ✅ Free, side-by-side DDL per object, generate script.
* ❌ Slow (web UI over Python), weak ordering, no data compare.

### pgcopydb / pg_dump -j / pgloader
* ✅ `pgcopydb`: parallel table copy using **exported snapshot** for consistency, split large tables
  across workers, indexes built in parallel after load. **We adopt snapshot export + table splitting.**
* ✅ `pgloader`: COPY streaming, continue-on-error batches.
* ❌ CLI only, Linux-centric, no diff UI.

### apgdiff / pgquarrel / sqitch / dbmate
* ✅ Script-first, versionable outputs. We support **export as migration file** (timestamped `.sql`)
  so output plugs into Supabase migrations, sqitch, Flyway, dbmate.

## 3. Feature matrix (target for Migrate Baba v1)

| Capability | Redgate | Navicat | DataGrip | pgAdmin | migra | pgcopydb | **Migrate Baba** |
|---|---|---|---|---|---|---|---|
| Native fast UI | ✔ (win) | ✔ | ✖ JVM | ✖ web | — | — | **✔ Rust/GPU** |
| Mac + Windows | ✖ | ✔ | ✔ | ✔ | ✔ | ✖ | **✔** |
| Tables/cols/constraints/indexes | ✔ | ✔ | ✔ | ✔ | ✔ | — | **✔** |
| Functions / procedures (RPC) | ~ | ~ | ✔ | ✔ | ✔ | — | **✔** |
| RLS policies, enable/force RLS | ✖ | ✖ | ~ | ✔ | ✔ | — | **✔** |
| Enums / domains / composite types | ~ | ~ | ✔ | ✔ | ✔ | — | **✔** |
| Views / materialized views w/ deps | ✔ | ~ | ✔ | ✔ | ✔ | — | **✔** |
| Partitioned tables | ~ | ✖ | ✔ | ✔ | ~ | ✔ | **✔** |
| Supabase managed-schema awareness | ✖ | ✖ | ✖ | ✖ | ✔ | ✖ | **✔** |
| Supabase Edge Functions diff | ✖ | ✖ | ✖ | ✖ | ✖ | ✖ | **v1.1 (Mgmt API)** |
| Hazard / destructive warnings | ✔ | ✖ | ~ | ✖ | ✖ | — | **✔** |
| Test deploy (tx + rollback) | ✖ | ✖ | ✖ | ✖ | ✖ | — | **✔** |
| Data compare (server-side hashing) | ✔ | ~ | ✖ | ✖ | ✖ | ✖ | **✔** |
| Data sync (upsert + delete) | ✔ | ✔ | ✖ | ✖ | ✖ | ✖ | **✔** |
| Parallel binary COPY + snapshot | ✖ | ✖ | ✖ | ✖ | ✖ | ✔ | **✔** |
| CLI for CI | ✔ | ✖ | ✖ | ✖ | ✔ | ✔ | **✔** |

## 4. UX principles distilled

1. **Compare first, act second.** Nothing touches the target until the user has seen the plan.
2. **One screen per job** — Connections, Schema Compare, Data Compare, Data Migration, History.
3. **The script is the truth.** Every action is shown as SQL that can be copied/saved/reviewed.
4. **Explain risk in plain words** next to the statement ("drops column `users.age` — data loss").
5. **Progress you can trust** — rows, bytes, throughput, ETA, per table; cancel anywhere.
6. **Remember everything** — saved connections, last comparison, run history with scripts and logs.
