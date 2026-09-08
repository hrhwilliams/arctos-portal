# Note 06 -- Stack review and alternatives

A review of the Rust + Elasticsearch + DuckDB/Parquet stack as it stands, with measurements taken on
2026-09-08 against the 2026-03-09 snapshot on the development machine (16 cores, 32 GB; Docker VM
15.5 GB), and an evaluation of the alternatives. Numbers are specific to this machine; the ratios are the
point, as in note 04.

**Short answer.** Elasticsearch is the right engine for the interactive search as specified, and nothing
measured here beats it with less work. The DuckDB/Parquet layer is not wrong, but it is a second store
whose stated justification no longer holds: the ES `_source` already carries all 154 columns, so the
export can come from ES alone. The leanest credible alternative is DuckDB as the *only* engine, which
a prototype shows serving every endpoint at 150-400 ms and exporting 100k rows in 1.3 s, at the cost of
one process sharing CPU between searches and exports. Postgres is the boring single-store option if
robustness outranks facet speed. Typesense, Meilisearch, ClickHouse, and Tantivy do not fit the nested
semantics this domain hinges on, or bring nothing the current engines lack.

---

## 1. What the stack does today

| Piece | Role | Size / cost |
| --- | --- | --- |
| `2026-03-09_filtered_flat.csv` | source dump, 5,771,371 rows x 154 columns | 50 GB |
| `ingest.py` | two Python passes over the CSV, builds nested docs, bulk-loads ES, force-merges | ~10 min pass 1 + ~15 min pass 2 + bulk + merge (measured 6,400 rows/s) |
| Elasticsearch 9.4 | search, funnel counts, guid enumeration for export | index 24.6 GB, 41.3M Lucene docs (5.77M roots + nested), 4 shards, 1 segment each; container 8.1 GB |
| `build_parquet.py` | rewrite CSV as Parquet, one directory per `guid_prefix`, all columns VARCHAR, then compact | 1.1 GB; ~20 min compaction |
| DuckDB (in-process) | startup facets and taxa (11 full passes), export rows by guid semi-join, `COPY ... csv.gz` | per-export in-memory connection, all 16 threads |
| axum service | `/api/search`, `/api/relations`, `/api/download`, `/api/schema`, `/api/taxa` | 4,500 lines Rust incl. tests |
| nginx + cloudflared + Kibana | TLS, rate limit on download, tunnel, and an admin UI | Kibana 866 MB idle |

The data flow for a download is: ES enumerates matching guids by `search_after` in 10k pages,
DuckDB reads the partitions those guids name and semi-joins the guid file, writes gzip CSV to `.tmp`,
the service reads it whole and sends it.

## 2. Measurements

### 2.1 Interactive search

| Query (shape emitted by `translate.rs`) | Elasticsearch | DuckDB on raw Parquet | DuckDB native tables (prototype) |
| --- | --- | --- | --- |
| genus Sorex, page 1 + funnel aggs | 90 ms warm / 170 cold | 190 ms (no funnel) | 150 ms funnel + 200 ms page |
| Sorex + attribute sex=male + aggs | 95 ms | 1,400 ms | 150 ms |
| sex=male alone, exact count | 7 ms | 3,900 ms | 180 ms |
| class Mammalia + sex=male (554k) | -- | -- | 380 ms |
| country facet | 2 ms | 180 ms | 100 ms (country+state+prefix) |
| locality phrase "Sandia Mountains" | 1 ms | 300 ms (ILIKE) | 300 ms (ILIKE) |
| date range 2020 | 0 ms | 190 ms (varchar compare) | not built (see 2.4) |
| relations tab, Sorex host of parasite class Cestoda | two-phase, pages whole matched set | -- | 125 ms as one JOIN |
| page 1 with attrs + relations re-attached | in `_source` | -- | 320 ms |
| interactive query while a heavy scan runs on another connection | unaffected (separate process) | -- | 930-1,330 ms (7x) |

ES wins on every interactive query and is unaffected by concurrent load. DuckDB over the raw
all-VARCHAR Parquet is not interactive-grade for attribute filters because every attribute row is a JSON
parse. DuckDB over normalized native tables is 2-4x slower than ES but still sub-half-second, until
something else is hammering the same process.

### 2.2 Export, 50,334 rows (note 04 benchmark: genus Myodes + Clethrionomys, 9 default columns)

| Path | Warm | Cold | Notes |
| --- | --- | --- | --- |
| Current: ES guids (docvalues) + DuckDB Parquet semi-join | 0.7-0.8 s + 1.2 s = ~2 s | ES half 6.5-32 s | note 04 measured 3.5-4 s before the force merge |
| ES `_source` only, 9 columns | 3.2 s | 12-32 s | 71 MB of JSON over the wire, CSV assembled in Rust |
| ES `_source` only, all 154 columns | 8.8 s | -- | 770 MB payload; note 04 measured 86 s via Parquet |
| DuckDB native table only, 9 columns | 1.1-1.4 s | same | 100k rows: 1.3 s; Parquet at 1.1 GB stays in cache |

### 2.3 What the cold numbers mean

The same ES export ran at 2.0 s, then 32 s, then 3.2 s within an hour, and by the end of the session
every ES path was 2-15x slower than at the start:

| ES path | healthy (start of session) | degraded (end of session) |
| --- | --- | --- |
| search page with funnel aggs, warm | 90 ms | 200-240 ms |
| 10k-guid export page, warm | 90 ms | 660-1,400 ms |
| 50k guids, docvalues | 0.7 s | 3.4-6 s (31k rows) |
| 50k rows, 9 columns from `_source` | 3.2 s | 26-51 s (93k rows) |

The node stats at the end explain it: the Docker VM reported 99% memory used, 3.0 GB of its 4 GB swap
in use, and the ES container had grown from 7.4 GB to 11.7 GB. The index is 24.6 GB, the heap is 8 GB,
and the VM has 15.5 GB, so the index can never be fully cached, and once the VM starts swapping the
heap itself pays. Two things pushed it there during this session: the DuckDB prototype holding 10 GB on
the host, and a `cargo clean` deleting 98 GB at the same time. Neither is a production event, but the
mechanism is: ES latency on this box depends on what else the host is doing. Note 04 already warned
about this trap when benchmarking. The Parquet copy is 1.1 GB and never falls out of cache, which is the
one structural advantage the DuckDB export path has. The healthy-state numbers are the ones used
elsewhere in this note.

### 2.4 Build cost of the DuckDB-only prototype

| Step | Time | Memory |
| --- | --- | --- |
| 17 search/export columns from Parquet into a native table | 11-15 s | under 10 GB |
| attributes (14.5M rows), relations (289k), agents (6.9M), taxon chain | 35 s | under 10 GB, checkpoint between tables |
| events from `json_locality` | failed at 10 GB with spill disabled | needs spill or fewer threads |
| all 154 columns into a native table with `ORDER BY guid` | 8 min, then killed | 45 GB of spill -- do not do this |
| resulting database (search columns + child tables) | 3.0 GB | -- |

The whole search model builds in under a minute from the Parquet. The wide columns should stay in
Parquet; native storage of the full dump buys nothing and costs an hour of I/O.

## 3. Verdict on the current stack

**Elasticsearch is the right choice for the search as specified.** The domain hinges on same-object
constraints on nested arrays: an attribute row is a (type, value) pair on one attribute record, a taxon
row's relationship and Related taxon must hold for the *same* relation. The funnel summary needs several
exact counts over sets wider than the result page. Locality is a phrase match on analysed text. ES has
native, indexed answers for all of these (`nested`, `global` + `filter` aggregations, `match_phrase`),
answers them in 90 ms, and isolates them from export load by being a separate process. Nothing else
evaluated does all four without either a workaround or a scan.

**The Rust service is fine.** It is thin, the query translation is well tested, the lints are strict,
and the export and relations code explain their own trade-offs. The problems in section 4 are gaps,
not design errors.

**The DuckDB/Parquet layer is the questionable part.** It exists to answer "what are these records"
because the note 04 assumption was that putting all columns in ES `_source` "costs an index roughly the
size of the dump." That assumption is already false: `ingest.py` does `parse_row(row)` over every
column, so `_source` holds all 154 dump columns plus the derived ones (160 keys measured). The index is
24.6 GB *with* them. The second store therefore buys a 1.2 s export instead of a 3.2 s one, and costs:
a second ingest script and format, a second copy of the data, the all-VARCHAR fidelity rules of note
04 section 6, eleven full scans at startup, the DuckDB C++ build in the Docker image, and a guid
hand-off between two systems that must agree on what a record is.

So: not the wrong stack, but one store heavier than the problem needs.

## 4. Problems found, independent of stack choice

Ordered by how much they matter in production.

1. **No export row cap is enforced.** `schema.rs` advertises `max_export_rows: 100000` and says
   `state.rs` enforces it. Nothing does. `export_guids` in `state.rs` pages until the index runs out, and
   an empty form is a `match_all` (`translate.rs` has a test named for it, with "the caller must
   refuse", and no caller refuses). `GET /api/download` with no parameters exports all 5.77M rows. The
   nginx rate limit is the only brake.
2. **The export is buffered whole in memory** before the first byte is sent (marked `ponytail:` in
   `export_csv_gz`). With `?cols=` allowing all 154 columns and no row cap, one request can hold
   hundreds of MB. Stream the file, or the ES pages, with `axum::body::Body::from_stream`.
3. **Every export opens a DuckDB connection with all 16 threads and no memory limit.** Two concurrent
   exports contend with each other and with the tokio runtime. There is no service-side concurrency
   limit; a `Semaphore` around the download handler and `SET threads` on the connection are a few lines.
4. **`/api/relations` pages the entire matched specimen set through the service** (10k docs per page,
   `_source: relations`) before it can render page 1, and only then fails if the result exceeds 65,536.
   A broad row like `||host of parasite` matches every host record in the snapshot. A nested `terms`
   or `composite` aggregation on `relations.related_guid` inside the index would return the same set
   without moving the documents. The `ponytail:` comment in `related_guids` already says so.
5. **ES memory is undersized for the index** (section 2.3): 24.6 GB of index, 8 GB of heap, a 15.5 GB
   VM with 4 GB of swap, and the swap was in use. Either the VM gets enough RAM to cache ~25 GB, or the
   index shrinks: `index.codec: best_compression`, `_source` `excludes` for the columns nobody exports
   by default (`media`, `json_locality`, `partdetail` are large and duplicated into nested fields), and
   `bootstrap.memory_lock: true` so the heap at least cannot be swapped.
6. **Startup runs eleven full Parquet passes** and rebuilds the same schema from the same immutable
   snapshot on every restart (note 04 section 8). Cache it to a JSON file keyed on the dataset, or read
   facets from ES `terms` aggregations as spec 03 originally specified (2 ms measured).
7. **Configuration is hard-coded**: the ES URL and the snapshot date live in `main.rs`. There is no
   request timeout on the `reqwest` client, so a hung ES holds a handler forever. Every search logs its
   full query at `info`.
8. **Kibana runs in the production compose file** for no user-facing purpose, at 866 MB.
9. **Ingest is ~30 minutes of single-threaded Python over 50 GB.** Pass 1 (the taxonomy map) is a
   10-minute CSV read that DuckDB answers in seconds from the Parquet; the whole `build_document` step
   could be a `COPY (SELECT ... ) TO 'ndjson'` feeding the bulk API.
10. **Nothing exercises the ES-facing code in tests.** The translate tests are thorough; the envelope
    decode has fixtures; the only end-to-end test is the ignored Parquet export test. A small fixed
    index (the 199-row sample the fixtures use) behind a `cargo test -- --ignored` would catch mapping
    drift.

## 5. Alternatives

### 5.1 Elasticsearch only: drop DuckDB and Parquet

Export from `_source` with `search_after`, streaming CSV from Rust. Measured 3.2 s warm for the 50k
benchmark with the 9 default columns and 8.8 s for all 154 columns. Facets and the taxa table come from
`terms` and `composite` aggregations. What goes away: `build_parquet.py`, the `duckdb` crate and its
C++ build, the 1.1 GB copy, the startup passes, the all-VARCHAR fidelity rules, and the two-system guid
hand-off. What it costs: the cold-cache variance of section 2.3 (fix with RAM or a smaller index), and
owning CSV assembly for wide rows, which `to_csv` in `routes/search.rs` already does for the search
path. GBIF does exactly this for small downloads and only falls back to Hive for large ones.

**Smallest change with the biggest simplification.** Take it if the box can hold the index in cache.

### 5.2 DuckDB only: drop Elasticsearch

The prototype in section 2 models the search columns as one native table with child tables for
attributes, relations, events, and agents, keeps the wide columns in the existing Parquet for export,
and expresses every endpoint as one SQL statement. The funnel is `COUNT(*) FILTER (WHERE ...)` x 4 in
one query. The relations tab is one join instead of the two-phase scan. Counts are always exact; there is
no `track_total_hits` cap. Export is the same engine, 1.3 s for 100k rows. Ingest is SQL from the
Parquet in under a minute instead of 30 minutes of Python. The JVM, the 8 GB heap, Kibana, and two
containers go away.

What it costs:

- **No inverted index.** Every filter is a scan with zone maps: 150-400 ms instead of 90 ms. Fine for
  the form; slower if the portal ever adds free-text search across many fields.
- **One process shares CPU.** A heavy concurrent scan pushed interactive latency to 1.1 s (7x). This is
  the real risk. Mitigations: a dedicated export connection with `SET threads = 4`, a semaphore on
  exports, or a second process for exports. Postgres and ES do not have this problem.
- **Text search is `ILIKE` or the FTS extension** (BM25 ranking, not phrase matching). Locality at
  300 ms via `ILIKE` is acceptable; a `spec_locality` child table of trigrams would make it fast.
- **Geo** is fine: `GEOMETRY` is a core type since DuckDB 1.5.
- **Build memory.** The child tables need ~10 GB or controlled spill. Build on the ingest box, ship the
  3 GB `.duckdb` file plus the Parquet.

**The leanest credible stack.** Roughly a rewrite of `translate.rs` into a SQL builder and of
`state.rs`; the tests carry over as behaviour specs. Take it if fewer moving parts outranks 90 ms
searches and the export contention is handled.

### 5.3 PostgreSQL, plain or with ParadeDB

Arctos itself is Postgres. Normalized child tables with btree/GIN indexes answer every filter here;
`tsvector` or `pg_trgm` covers locality; PostGIS covers maps; `COPY (query) TO STDOUT CSV` streams the
export with correct quoting; MVCC gives real per-query isolation, the thing DuckDB lacks. ParadeDB adds
a Tantivy BM25 index with index-level facets to the same database, which is its whole pitch:
"search without a second system."

Costs: `count(*)` over millions of rows is seconds, not milliseconds, without extra work, so the
funnel counts on broad queries will feel slower than ES; a 50 GB CSV load is tens of minutes; one more
server, though the most boring one available. Not measured here.

**The robust single store.** Take it if the snapshot model ever turns into live updates from Arctos,
or if operational familiarity outranks facet speed.

### 5.4 OpenSearch

Same query DSL, same nested and aggregation semantics, same code. A licensing or vendor decision, not a
performance one. Nothing to gain here.

### 5.5 Typesense or Meilisearch

Excellent typeahead and facets with tiny operational cost, but the domain's core constraint, that two
conditions hold on the *same* element of an array of objects, is not expressible in Meilisearch
(open discussion since 2022) and only recently in Typesense via scoped `field.{...}` filters, which
have had hangs and bugs on nested arrays in v29. The universal workaround, pre-composing tokens like
`sex=male` and `host of parasite|cestoda` at ingest, works in any engine and is not a reason to switch.
Neither has an export path or a funnel-style multi-count. Not a fit as the main engine. Possibly the
taxa typeahead, if that ever needs more than the in-memory prefix scan.

### 5.6 Tantivy embedded, Quickwit

Tantivy inside the Rust binary removes the JVM and the ES container, but has no nested documents or
block joins, so the same pre-composed token trick applies, and the service would own segment
management, mapping, and aggregations that ES gives for free. Quickwit is built for logs on object
storage. Only worth it if "one static binary" is the goal, and then DuckDB-only gets there with less
code.

### 5.7 ClickHouse

`arrayExists` gives same-element semantics natively, facets are fast, CSV output is built in. It is a
heavier server than 5.7M rows needs, and DuckDB delivers the same columnar wins in-process. Skip.

### 5.8 SQLite with FTS5

Would work at this size in one file with WAL readers giving real query isolation. Row-oriented, so facet
counts and wide scans are slower than DuckDB, and there is no `COPY ... gzip`. Only if minimal ops is
the sole priority. DuckDB dominates it for this shape.

### 5.9 What the field runs

GBIF serves occurrence search from Elasticsearch, small downloads straight from ES, and large downloads
via Hive or Spark over Parquet and HBase. iDigBio runs Elasticsearch beside Postgres. The current
architecture is the standard one at 500x this scale. At 5.7M rows the split is optional, not required.

## 6. Recommendation

1. **Now, in days, keep ES and close section 4 items 1-5.** Row cap, streamed response, export
   concurrency and thread limits, relations via an in-index aggregation, and RAM or index size for the
   cache. These matter whatever engine wins.
2. **Then pick one export source.** Either (a) ES `_source` and delete DuckDB, Parquet, and
   `build_parquet.py`, or (b) keep the Parquet path. (a) if the box can cache the index; it removes
   a store, a script, a format, and a hand-off. (b) if RAM is fixed; it is 1.2 s instead of 3.2 s and
   never cold. In both cases stop rebuilding facets from Parquet at startup.
3. **If simplicity is the goal, in weeks: DuckDB only** per section 5.2, with exports on a capped
   connection. Halves the containers, replaces the Python ingest with SQL, removes the JVM, and keeps
   every query under half a second.
4. **If live updates from Arctos ever arrive: Postgres**, optionally with ParadeDB. Not before.

Not recommended: Typesense or Meilisearch as the main engine, ClickHouse, Tantivy from scratch, or
OpenSearch for performance reasons.

## 7. Method

Four scripts ran from the session scratchpad and are not in the repository: an ES query benchmark
using the shapes `translate.rs` emits; an ES export benchmark paging `_source` with `search_after`;
a DuckDB benchmark over the raw Parquet; and the DuckDB-only prototype (native table of 17 columns,
child tables from the JSON columns, funnel, paging, relations join, export, and a contention test).
The prototype database was deleted. Every ES number was taken at least twice; the cold and warm figures
are reported separately because they differ by 5-15x on this machine.

Sources consulted for the alternatives:
[DuckDB full-text search](https://duckdb.org/docs/current/core_extensions/full_text_search),
[DuckDB 1.5 spatial as core](https://medium.com/@Praxen/duckdb-extensions-youll-actually-use-in-2026-bd0ea86a359f),
[ParadeDB pg_search](https://www.paradedb.com/blog/introducing-search),
[ParadeDB faceting](https://www.paradedb.com/blog/faceting),
[Typesense nested filter syntax](https://typesense.org/docs/guide/tips-for-filtering.html),
[Typesense #2469 nested array filter hang](https://github.com/typesense/typesense/issues/2469),
[Meilisearch discussion #675 on per-item nested filters](https://github.com/orgs/meilisearch/discussions/675),
[GBIF occurrence search and download](https://github.com/gbif/occurrence),
[GBIF occurrence index](https://www.gbif.org/article/6eufZyV17ykqiasMUu8yuO/gbif-infrastructure-occurrence-index).
