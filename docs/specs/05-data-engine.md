# Data Engine — copy, compare, sync

## 1. Session normalisation (both sides, every data connection)

```sql
SET DateStyle = 'ISO, MDY'; SET IntervalStyle = 'postgres'; SET TimeZone = 'UTC';
SET extra_float_digits = 3; SET bytea_output = 'hex'; SET client_encoding = 'UTF8';
SET application_name = 'migrate-baba';
```
Guarantees identical text representations for hashing and lossless text-format COPY.

## 2. Copy (`data::copy`)

### 2.1 Job planning
1. Introspect both sides (reuses schema engine).
2. For each selected table present in both: column list = source columns ∩ target columns (by name),
   excluding target **generated** columns. Missing/extra columns are warnings.
3. Format: **binary** if every copied column has the same `data_type` on both sides *and* the type is a
   built-in (`oid < 16384`; user-defined enum/composite/array OIDs differ between databases and break
   binary array/record receive). Otherwise **text**.
4. Split: if `pages > split_threshold` (default 64 MiB / 8 KiB = 8192 pages) and source ≥ PG14 →
   `k = min(workers, ceil(pages / split_threshold))` ctid ranges
   `ctid >= '(a,0)' AND ctid < '(b,0)'` (last range open-ended). Each range is an independent work item.
5. Order work items largest-first (LPT scheduling) so the long tail is short.

### 2.2 Execution
* **Snapshot**: a coordinator connection runs `BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY;
  SELECT pg_export_snapshot();` and stays open; each source worker does
  `BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY; SET TRANSACTION SNAPSHOT '<id>';`.
  On failure (standby, pooler) continue without snapshot, emitting a warning.
* **Target session**: try `SET session_replication_role = replica` (disables FK & user triggers);
  if refused, compute FK dependency **waves** (topological levels) and only start a wave when the
  previous one completed.
* **Replace mode**: one `TRUNCATE t1, t2, … ` (all selected tables, single statement so mutual FKs are
  fine) before any copy.
* Per work item: `BEGIN` on target →
  `COPY src_cols FROM STDIN (FORMAT binary|text)` ←stream← `COPY (SELECT cols FROM t [WHERE ctid range]) TO STDOUT (FORMAT …)` → `COMMIT`.
  Bytes are forwarded chunk-by-chunk (`Bytes`, no parsing, no row materialisation) — throughput is bound by
  network, not CPU. Back-pressure is natural (sink awaits).
* Workers: `workers` long-lived (source, target) connection pairs pulling from a shared queue.
* **Progress**: atomic `bytes`/`rows` counters per item; rows known exactly at `COMMIT` (COPY returns
  count); in-flight rows estimated from bytes × (est_rows / est_bytes).
* **Post-load**: `setval` for every sequence owned by / identity of a copied column:
  `SELECT setval(seq, COALESCE(max(col), 1), max(col) IS NOT NULL) FROM t`.
  Then `ANALYZE` copied tables (option, default on).

### 2.3 Failure handling
* Each work item is its own transaction → failure rolls back only that item; others continue
  (unless *stop on first error*). Error shows table, SQLSTATE, message.
* Cancel: token checked per chunk; in-flight transactions roll back; coordinator closes.

## 3. Compare (`data::compare`)

### 3.1 Tables with a primary key
1. **Boundaries** (source): every `chunk_size`-th key (default 10 000 rows):
   `SELECT <pk literals> FROM (SELECT pk…, row_number() OVER (ORDER BY pk…) rn FROM t) s WHERE rn % N = 0`.
   Literals are produced server-side with `format('%L', col)` and cast back with `::type`.
2. Chunks = `(-∞, b1), [b1, b2), …, [bn, +∞)` — covers the whole key space on **both** sides, so rows
   existing only in target are caught.
3. **Hash** each chunk on both sides concurrently:
   `SELECT count(*), md5(string_agg(md5(ROW(c1,…)::text), '' ORDER BY pk…)) FROM t WHERE <range>`.
   Only 2 small values per chunk cross the network.
4. Chunks with different (count, hash) → **drill-down**: stream `(ROW(pk…)::text, md5(ROW(c…)::text))`
   ordered by pk from both sides, merge-join in Rust → only-in-source / only-in-target / different keys.
   Up to `sample_limit` (default 200) keys kept per category for display; counts are exact.

### 3.2 Tables without a PK
`count(*)` + `md5(string_agg(md5(ROW(*)::text), '' ORDER BY md5(ROW(*)::text)))` — order-independent hash.
Result: identical / different (no row-level detail).

## 4. Sync (`data::sync`)
For each mismatched chunk (or the whole table when no chunk info), on target in one transaction:
```sql
CREATE TEMP TABLE _mb_stage ON COMMIT DROP AS SELECT <cols> FROM <t> WITH NO DATA;
COPY _mb_stage (<cols>) FROM STDIN (FORMAT …)          -- streamed from source range
DELETE FROM <t> t WHERE <range on t> AND NOT EXISTS (SELECT 1 FROM _mb_stage s WHERE (s.pk…) = (t.pk…));
WITH up AS (
  INSERT INTO <t> (<cols>) OVERRIDING SYSTEM VALUE SELECT <cols> FROM _mb_stage
  ON CONFLICT (<pk>) DO UPDATE SET c = EXCLUDED.c, …
  WHERE (<t>.c, …) IS DISTINCT FROM (EXCLUDED.c, …)
  RETURNING (xmax = 0) AS inserted)
SELECT count(*) FILTER (WHERE inserted), count(*) FILTER (WHERE NOT inserted) FROM up;
```
Result per table: inserted / updated / deleted counts. Tables without a PK: sync = replace.
