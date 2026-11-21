# Migrate Baba — Product Overview

> A native, Rust-powered desktop app (Windows + macOS, Linux as a bonus) for **comparing, diffing and
> migrating PostgreSQL databases** — self-hosted Postgres, Supabase, Neon, AWS RDS/Aurora, Google Cloud SQL,
> Azure Flexible Server, DigitalOcean, Render, Railway, Crunchy Bridge, Timescale and any other
> wire-compatible Postgres (v12+).

## 1. Problem

Teams moving between Postgres environments (dev → staging → prod, Supabase project → Supabase project,
self-hosted → cloud, cloud → cloud) juggle `pg_dump`, `migra`, hand-written SQL and ad-hoc scripts.
The good GUI tools (Redgate, Navicat, DataGrip, DBeaver Pro) are expensive, slow (JVM/Electron), or weak
on Postgres-specific objects (RLS policies, functions/RPC, extensions, enums, partitions, identity columns).

## 2. Vision

One fast native app where you:

1. **Connect** a *source* and a *target* (paste a URL or use a provider preset).
2. **See every difference** — schema objects and data — side by side, in seconds, even across oceans.
3. **Pick** what to apply, **review** the generated SQL (destructive steps flagged in red),
   **test-deploy** it inside a rolled-back transaction, then **deploy** it.
4. **Copy / sync data** at wire speed with parallel binary `COPY` streams and a consistent snapshot.

## 3. Goals (v1)

| # | Goal | Measured by |
|---|------|-------------|
| G1 | Schema compare for all common Postgres object types (see `02-functional-spec.md` §3) | Object coverage matrix |
| G2 | Correctly ordered, dependency-aware migration script (create/alter/drop) | Generated script applies cleanly on target |
| G3 | Side-by-side DDL diff per object, filterable tree, select/deselect per object | UX review |
| G4 | Data compare (row counts + chunked hashes + row-level drill-down) without pulling whole tables to the client | Bytes on wire ≪ table size |
| G5 | Data migration: replace / append / sync (upsert + delete) with parallelism | ≥ 80% of network bandwidth on large tables |
| G6 | Native, low latency UI: never blocks on I/O, 60 fps, ≤ 300 ms cold start | Perf budget (`07-performance.md`) |
| G7 | Safe by default: dry run, test-deploy (rollback), destructive-op guard, credentials in OS keychain | Security checklist |

## 4. Non-goals (v1)

* Non-Postgres engines (MySQL, SQL Server …). The engine is abstracted, but only Postgres ships.
* Continuous CDC replication (logical replication streaming) — roadmap v2.
* Role/user management and cluster-level objects (tablespaces, databases).
* Becoming a general SQL IDE (a small read-only query console is in roadmap).

## 5. Personas

* **Indie / startup dev on Supabase** — wants to push local/staging schema (tables, RLS, RPC functions)
  to production and clone data to a new project. Values: simplicity, safety, Supabase awareness.
* **Backend / platform engineer** — migrates between cloud providers (RDS → Neon, self-hosted → Cloud SQL);
  values throughput, consistency, resumability, a CLI for CI.
* **DBA / reviewer** — wants an exact, reviewable SQL script and a diff report to attach to a change ticket.

## 6. Spec index

| File | Contents |
|------|----------|
| `01-competitive-analysis.md` | What the best paid + free tools do, and what we adopt |
| `02-functional-spec.md` | Feature requirements with IDs (FR-xxx) |
| `03-architecture.md` | Crates, threading model, data flow, error model |
| `04-schema-engine.md` | Catalog model, introspection queries, diff algorithm, DDL generation & ordering |
| `05-data-engine.md` | Data copy, data compare, data sync algorithms |
| `06-ui-ux.md` | Screens, workflows, keyboard shortcuts, visual language |
| `07-performance.md` | Latency/throughput budgets and the techniques that meet them |
| `08-security-safety.md` | Credentials, TLS, destructive-operation guards, audit history |
| `09-testing-roadmap.md` | Test strategy, release plan, roadmap |
