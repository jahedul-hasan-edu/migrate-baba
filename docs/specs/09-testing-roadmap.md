# Testing & Roadmap

## 1. Test strategy

| Layer | What | How |
|---|---|---|
| Unit | quoting, URL parsing, DDL generation, diff detection, plan ordering, hazard tagging | `cargo test` on in-memory `Catalog` fixtures |
| Integration | introspect → plan → apply → re-introspect = no diff ("round-trip" property) | Gated on `MB_TEST_SOURCE_URL` / `MB_TEST_TARGET_URL` (two empty databases); runs fixture SQL covering every object kind |
| Data | copy/compare/sync on generated tables (1 M rows, all common types incl. arrays, json, enums, bytea) | same env vars |
| Provider matrix | Supabase, Neon, RDS, local PG 12–17 | manual + nightly CI with Docker `postgres:12…17` |
| UI | smoke: app launches, screens render with fake state | `cargo test -p mb-app` (headless egui `Context::run`) |
| Performance | introspection of 2 000-table schema, copy 10 GB | benchmark scripts, tracked per release |

Round-trip invariant: **for any fixture F: apply(plan(F_source → empty)) to empty ⇒ diff(F_source, result) = ∅.**

## 2. Release plan

| Milestone | Content |
|---|---|
| **M1 – Core (this iteration)** | Workspace, connection/TLS, introspection, diff, plan with hazards, deploy/test deploy, parallel copy, data compare + sync, egui app with all screens, CLI |
| M2 – Polish | Privileges diff, rename hints, migration-file export presets, report export, resume copy, installers (MSI via cargo-wix, DMG/notarisation via cargo-bundle) |
| M3 – Supabase+ | Edge Functions diff/deploy (Management API), storage buckets, pg_cron jobs, publications |
| M4 – Online | `CREATE INDEX CONCURRENTLY`, `NOT VALID` constraints, logical-replication CDC for near-zero-downtime cutover |
| M5 – Engines | Dialect trait; MySQL/MariaDB source |

## 3. Packaging
* Windows: `cargo build --release` → `migrate-baba.exe`; MSI via `cargo-wix` (M2). Code signing.
* macOS: universal binary (`aarch64` + `x86_64` via `lipo`), `.app` bundle, notarised DMG (M2).
* Auto-update via GitHub Releases (M2).
