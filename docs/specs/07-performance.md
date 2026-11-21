# Performance Specification

## 1. Budgets

| Metric | Budget | Technique |
|---|---|---|
| Cold start to interactive | ≤ 300 ms | Native binary, no runtime/VM, lazy keychain reads |
| Idle CPU | ≈ 0 % | Repaint only on input or events (`request_repaint`) |
| Frame time with 50 k diff rows | ≤ 8 ms | Virtualised lists (`show_rows`), pre-computed display strings |
| Introspection, 1 000 tables, 100 ms RTT | ≤ 1.5 s | Simple-query pipelining (all catalog queries in flight at once), both sides in parallel |
| Diff + plan of 1 000 tables | ≤ 50 ms | BTreeMap merges, no regex, string compares |
| Copy throughput | ≥ 80 % of link bandwidth | Binary COPY passthrough, parallel workers, ctid splitting, snapshot |
| App memory during copy | O(workers × chunk) ≈ < 100 MB | Streaming `Bytes`, no buffering of tables |
| Data compare traffic | O(#chunks) bytes for identical tables | Server-side md5 aggregation |
| Cancel latency | ≤ 200 ms | Cancellation token checked per network chunk |

## 2. Techniques

1. **Pipelining** — tokio-postgres sends queries without waiting for previous responses when futures are
   polled concurrently (`try_join!`). Introspection = ~1 RTT + server time.
2. **Zero-parse copy** — `COPY TO STDOUT` chunks are forwarded as-is to `COPY FROM STDIN`.
3. **Binary format** when safe (smaller on the wire, no text parsing on target).
4. **Parallelism** — N connection pairs; large tables split into ctid ranges (PG14 TID Range Scan).
5. **LPT scheduling** — largest work items first minimises the makespan.
6. **Server-side hashing** — compare without transferring rows.
7. **Atomics for progress** — no per-chunk channel messages.
8. **Release profile** — `opt-level=3`, `lto="thin"`, `codegen-units=1`, `panic` unwind kept for
   worker isolation; `strip = true`.
9. **Allocation discipline** — DDL strings pre-built once per compare; UI uses `&str` views.
