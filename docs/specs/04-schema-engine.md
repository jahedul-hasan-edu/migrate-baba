# Schema Engine — model, introspection, diff, plan

## 1. Catalog model (`mb_core::model`)

All identities are *qualified names* `QName { schema, name }` and maps are `BTreeMap` for deterministic
output. Functions are keyed by `schema.name(identity_args)` (overloads are distinct objects).

```
Catalog
 ├─ server_version: i32
 ├─ schemas:    BTreeMap<String, SchemaDef{ name, comment }>
 ├─ extensions: BTreeMap<String, ExtensionDef{ name, schema, version }>
 ├─ types:      BTreeMap<QName, TypeDef{ kind: Enum{labels} | Domain{base, not_null, default, checks} | Composite{attrs}, comment }>
 ├─ sequences:  BTreeMap<QName, SequenceDef{ data_type, start, increment, min, max, cache, cycle, owned_by }>
 ├─ tables:     BTreeMap<QName, TableDef{
 │                 columns: Vec<Column{ name, data_type, type_oid, not_null, default, identity, generated, collation, comment }>,
 │                 constraints: BTreeMap<String, Constraint{ kind: P|U|C|F|X, definition, ref_table }>,
 │                 indexes: BTreeMap<String, Index{ definition }>,
 │                 triggers: BTreeMap<String, Trigger{ definition, enabled }>,
 │                 policies: BTreeMap<String, Policy{ permissive, roles, cmd, using, with_check }>,
 │                 rls_enabled, rls_forced, unlogged, partition_key, partition_of, partition_bound,
 │                 comment, est_rows, total_bytes, pages }>
 ├─ views:      BTreeMap<QName, ViewDef{ materialized, definition, columns, depends_on: Vec<QName>, comment }>
 └─ functions:  BTreeMap<String /*schema.name(args)*/, FunctionDef{ schema, name, identity_args, kind: Function|Procedure,
                   result, language, definition, comment }>
```

`type_oid`, `est_rows`, `total_bytes`, `pages` are **informational** and never compared.

## 2. Introspection

* Session: `SET search_path = ''` so `format_type`, `pg_get_*def` emit fully-qualified names → output is
  comparable between databases. Also `SET statement_timeout`, `application_name = 'migrate-baba'`.
* All queries use the **simple query protocol** (`simple_query`) → works through PgBouncer/Supavisor,
  no prepared-statement state, text results.
* The ~12 catalog queries are issued **concurrently on one connection**; tokio-postgres pipelines them, so
  introspection costs ≈ 1–2 network round trips regardless of how many queries.
* Objects owned by extensions (`pg_depend.deptype = 'e'`) are excluded; `pg_*`, `information_schema` are
  always excluded; provider-managed schemas excluded by preset.
* Version gates: PG12+ required (`attgenerated`, `prokind`); PG13+ adds `tgparentid` filter.

Queries (abridged; full text in `introspect.rs`):

| Object | Catalog source |
|---|---|
| schemas | `pg_namespace` |
| extensions | `pg_extension` |
| enums | `pg_type` + `pg_enum` (`json_agg` labels by `enumsortorder`) |
| domains | `pg_type typtype='d'` + `pg_constraint contypid` |
| composites | `pg_type typtype='c'` with `pg_class.relkind='c'` |
| sequences | `pg_sequence` + `pg_depend` (owned-by `a`, identity `i` filtered) |
| tables | `pg_class relkind in (r,p)` + partition info `pg_get_expr(relpartbound)`, `pg_get_partkeydef` |
| columns | `pg_attribute` + `pg_attrdef` + `pg_collation` |
| constraints | `pg_constraint` (`pg_get_constraintdef`), `conparentid = 0` (skip inherited on partitions) |
| indexes | `pg_index` not backing a constraint, not a partition child index (`pg_get_indexdef`) |
| triggers | `pg_trigger` not internal (`pg_get_triggerdef`, `tgenabled`) |
| policies | `pg_policies` |
| views | `pg_class relkind in (v,m)` + `pg_get_viewdef(oid, true)`; dependencies from `pg_depend` ⨝ `pg_rewrite` |
| functions | `pg_proc prokind in (f,p)` + `pg_get_functiondef`, `pg_get_function_identity_arguments`, `pg_get_function_result` |

## 3. Diff (`mb_core::diff`)

For every object kind, keys are compared:
* key only in source → `Added` (will be **created** on target)
* key only in target → `Removed` (will be **dropped** from target)
* key in both, normalised definitions differ → `Changed`, with human-readable `details`
* otherwise `Identical`

Normalisation: trim trailing whitespace per line, unify line endings; optional ignore comments.
`ObjectDiff { kind, key, display, status, source_ddl, target_ddl, details, hazards }` drives the UI.

## 4. Plan generation (`mb_core::plan`)

Input: `current` (target), `desired` (target with selected source objects), options.
Output: ordered `Step { phase, sql, object, hazards: Vec<Hazard>, transactional: bool }`.

### 4.1 Phases (executed in this order)

| # | Phase | Statements |
|---|---|---|
| 0 | Pre | `SET check_function_bodies = off;` (only in script header) |
| 1 | Create schemas | `CREATE SCHEMA` |
| 2 | Extensions | `CREATE EXTENSION IF NOT EXISTS … WITH SCHEMA …`, `ALTER EXTENSION … UPDATE TO` |
| 3 | Types | `CREATE TYPE … AS ENUM / AS (…)`, `CREATE DOMAIN`, `ALTER TYPE … ADD VALUE`, alter domain/composite |
| 4 | Sequences | `CREATE SEQUENCE`, `ALTER SEQUENCE` |
| 5 | Drop dependants | drop views (changed/removed + transitive dependants of changed tables/views), triggers, policies, FKs, constraints & indexes that change or vanish |
| 6 | Drop functions | removed functions, functions whose result/kind changed |
| 7 | Tables | `CREATE TABLE` (parents before partitions), column add/alter/drop, LOGGED/UNLOGGED |
| 8 | Functions | `CREATE OR REPLACE FUNCTION/PROCEDURE` (new + changed) — after tables so RPCs returning `SETOF table` work; bodies are not validated (`check_function_bodies = off`) |
| 9 | Constraints & indexes | PK/UNIQUE/CHECK/EXCLUDE (new tables: inline), `CREATE INDEX` (optionally `CONCURRENTLY`, after the transaction) |
| 10 | Foreign keys | `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY`; unchanged FKs referencing a recreated PK/UNIQUE are dropped and re-added |
| 11 | Views | `CREATE VIEW` / `CREATE MATERIALIZED VIEW` in topological order of `depends_on` |
| 12 | Late functions | functions whose signature references a view |
| 13 | Triggers, RLS & policies | `ENABLE/FORCE ROW LEVEL SECURITY`, `CREATE POLICY`, `CREATE TRIGGER`, trigger enable state |
| 14 | Sequence ownership | `ALTER SEQUENCE … OWNED BY` |
| 15 | Drops | removed tables (children before parents), sequences, types, extensions, schemas |
| 16 | Comments | `COMMENT ON …` changes |

`ALTER TYPE … ADD VALUE` runs *before* the main transaction (autocommit) so later steps can use the new
label; `CREATE INDEX CONCURRENTLY` runs *after* it. Test deploy runs everything that can run in a
transaction and rolls back.

### 4.2 Alter rules

| Change | SQL | Hazards |
|---|---|---|
| column added | `ADD COLUMN` (with default/identity/generated) | `MAY_FAIL` if NOT NULL without default on non-empty table |
| column dropped | `DROP COLUMN` | `DATA_LOSS` |
| type changed | `ALTER COLUMN … TYPE t USING col::t` | `TABLE_REWRITE`, `MAY_FAIL` |
| default changed | `SET DEFAULT` / `DROP DEFAULT` | — |
| not null set | `SET NOT NULL` | `MAY_FAIL`, `LOCKS_TABLE` |
| identity changed | `ADD GENERATED … AS IDENTITY` / `DROP IDENTITY` / `SET GENERATED` | — |
| generated expr changed | drop + re-add column | `DATA_LOSS` (recomputed) |
| constraint changed | `DROP CONSTRAINT` + `ADD CONSTRAINT` | `MAY_FAIL` |
| index changed | `DROP INDEX` + `CREATE INDEX` | `LOCKS_TABLE` unless concurrently |
| partition key changed | — | `MANUAL` (commented out) |
| enum label removed/reordered | — | `MANUAL` |
| view changed | drop + create (dependants too) | — |
| function result/kind changed | `DROP FUNCTION` + create | `MAY_FAIL` (dependants) |
| table dropped | `DROP TABLE` | `DATA_LOSS` |

### 4.3 Identifier quoting
`quote_ident`: bare if `^[a-z_][a-z0-9_$]*$` and not a reserved keyword, otherwise double-quoted with
`"` doubled. Literals: `'…'` with `'` doubled (standard_conforming_strings assumed on, as in PG ≥ 9.1).

## 5. Deploy (`mb_core::deploy`)
* **Test deploy**: `BEGIN; <transactional steps>; ROLLBACK;` — reports the first failing step with
  SQLSTATE/detail/hint. Non-transactional steps are listed as "not tested".
* **Deploy**: `BEGIN; … COMMIT;` (or per statement), then non-transactional steps one by one.
  Progress event per step. Cancel → `ROLLBACK`.
* `lock_timeout` (default 10 s) set for deploy sessions so a blocked `ALTER TABLE` fails fast instead of
  queueing behind long transactions and blocking production traffic.
