# Note 04 — Export performance

Why `/api/download` is built the way it is. Everything here was measured on the 2026-03-09
snapshot (5,771,371 rows, 154 columns) against a local Elasticsearch and the Parquet dataset on
the same machine. Numbers are that machine's; the ratios are the point.

**Where it ended up.** A 50,335-row export across 44 collections (`genus:Myodes` +
`genus:Clethrionomys`, the benchmark query) went from **60.1s to ~3.5-4s warm** — ~1.2s of DuckDB
and ~3s of Elasticsearch. The path is: ES answers *which* records match, DuckDB reads *what* they
are out of Parquet and writes the gzip.

It was 22.7s (18s ES, 4.2s DuckDB) until three changes landed, none of which altered the
architecture: force-merging the index to one segment (§5), `track_total_hits: false` on export
pages (§5), and compacting the Parquet to one file per partition (§3).

---

## 1. The single-file Parquet was the original problem

`/api/download` took ~10s regardless of how much it exported. Thirty rows cost the same as
thirty thousand, which is the signature of a full scan.

| query on the 940 MB single file | time |
| ------------------------------- | ---- |
| `guid = 'x'` (one value)        | 0.43s |
| `guid IN (2)`                   | 8.6s |
| `guid IN (30)`                  | 9.7s |
| `guid_prefix = 'MSB:Mamm' AND guid IN (30)` | 9.3s |
| `count(*)` reading only the guid column | 0.07s |

**DuckDB pushes a single `=` into the Parquet scan but not a disjunction.** With one equality the
scan skips non-matching rows before decoding the other 153 columns. The moment the filter is
`IN`, `OR`, or a semi join — which is exactly what an export is — it materialises everything and
filters afterwards. Two guids cost what two million would.

The guid-only scan at 0.07s proves the file itself is fast to read. The cost is decoding columns.

### What did not work

- **Sorting by guid.** Shrank the file (214 MB → 108 MB on a subset — sorted data compresses
  well) and did not fix the lookup. No row-group pruning happens for a disjunction whatever the
  order.
- **Bloom filters.** There are none in the file (0 of 48 row groups carry one), which is why the
  single `=` fast path is zone maps rather than a probe, and why it doesn't extend to lists.
- **Dropping compression.** Uncompressed (3.2 GB) read in 1.06s against zstd-9 (214 MB) at 1.07s
  on identical data. Decompression is not on the critical path. Snappy was marginally slower.
  **Never spend disk on this.**
- **A DuckDB table with an ART index on guid.** Point lookups work but scale with guid count —
  1 guid 0.078s, 30 guids 0.53s, 300 guids 3.1s, i.e. ~10ms each, so 100k guids would be ~15
  minutes. It also cost 4.6 GB for 481k rows, extrapolating to ~56 GB.

---

## 2. Partitioning by `guid_prefix` (D-parquet-1)

The only lever that moves is **reading fewer rows**, and the cheapest filter is a filename.
`build_parquet.py` writes one directory per collection:

```
arctos_parquet/guid_prefix=MSB%3AMamm/data_0.parquet   (116 MB, 355k records)
arctos_parquet/guid_prefix=UAM%3AEnto/data_0.parquet   (90 MB)
...                                                     294 collections
```

The colon is URL-encoded, which is what makes it work on Windows, and `hive_partitioning = true`
decodes it on read, so `WHERE guid_prefix = 'MSB:Mamm'` still matches. `guid_prefix` is **not**
stored inside the files — every reader must pass that option or the column is missing.

`export_csv_gz` derives the prefixes from the guids themselves (`MSB:Mamm:12345` carries its own
collection) and adds `WHERE p.guid_prefix IN (…)`. Same 30 guids, with and without that clause:

| | time |
| --- | ---- |
| pruned to one collection | 2.22s |
| no prefix filter | 11.35s |

Partition **size** barely matters once pruned — a 2 MB collection and a 122 MB one land within
0.3s of each other, because ~1.8s of any export is fixed cost.

### What partitioning does not fix

Pruning selects whole collections and then **reads each matched collection in full**. A search
for a genus spans 40+ mammal collections, so it approaches the cost of reading everything. That
is the design's floor, and it is why the column list (§4) mattered more in the end.

---

## 3. Costs the layout introduced

**Fragmentation.** The writer emits a new file per partition every time it flushes, so the first
build produced **7,945 files across 294 partitions** (~27 each). That costs ~0.95s of glob and
footer reads on every query, against 0.18s for a ~300-file layout. It is also paid eleven times
at startup, once per full-dataset pass (`build_taxa` does one scan per rank, plus three facet
scans), which is why startup got noticeably slower.

**Fixed.** `build_parquet.py` now runs a `compact()` pass after the partitioned write: one `COPY`
per fragmented partition, reading its own files back and rewriting a single `data_0.parquet`. A
per-partition loop rather than a bigger `partitioned_write_flush_threshold`, because a threshold
large enough for the 355k-row largest partition is exactly the memory profile that OOM'd. It reads
*without* `hive_partitioning`, so the prefix stays in the directory name and no stray column lands
in the file. `python build_parquet.py --compact [dir]` runs it standalone against an existing
dataset — a read-and-rewrite, no CSV re-parse.

8,056 files → 300, 5,771,371 rows unchanged, ~20 minutes. The benchmark export's DuckDB half went
**4.2s → 1.2s** — more than the ~0.8s of glob predicted, since the footers are also read on the
matched partitions rather than 27 times each.

**Memory during the write.** The default `partitioned_write_flush_threshold` is 524,288 rows
buffered *per partition per thread*; across ~300 collections of 154-column rows that OOM'd at
15 GB. `build_parquet.py` sets it to 100,000 with `partitioned_write_max_open_files = 25`. Raising
it back means fewer files but more memory — that is the whole trade.

---

## 4. Columns are the dominant cost (D-parquet-2)

For 50,334 rows across 45 collections, holding everything else constant:

| | time |
| --- | ---- |
| find the rows (join only, no columns) | **2.2s** |
| + all 154 columns → Parquet (no CSV formatting) | 40.2s |
| + all 154 columns → csv.gz | 85.9s |
| + 7 columns → csv.gz | **2.2s** |

Finding rows is free. Materialising columns is everything, and it is roughly linear in column
count: scattered rows touch nearly every row group in the matched collections, so all 154 columns
get read whether or not the export wants them.

`EXPORT_COLUMNS` in `state.rs` is therefore deliberately short:

```
guid, scientific_name, country, state_prov, use_license_url, attributedetail, related_record_cache
```

Note the download is **not** the same field set as the search page. `events`, `relations`,
`event_date_min` and `event_date_max` are built by the ETL for Elasticsearch and do not exist in
the dump; `related_record_cache` is the nearest flat equivalent to `relations`, not the same
thing. Adding columns costs time proportionally — that is the trade to weigh if anyone asks for
the full record back.

**Output compression is not worth tuning.** Parquet zstd → CSV gzip is not a wasteful re-encode
that could be avoided: the two compress unrelated things (binary column chunks vs row-oriented
text), so the data must be decoded and re-encoded whatever codecs are chosen. gzip on the output
is also a net *win* — writing 39 MB beat writing 693 MB uncompressed — and it is what a browser
and Excel open without argument.

---

## 5. The Elasticsearch half

`export_query` in `translate.rs` reuses the search's bool query with `search_after` instead of
`from` (an export routinely runs past the 10,000-document result window) and fetches nothing but
the guid.

**`_source: false` + `docvalue_fields: ["guid"]` was a real win** — ~24s to ~18s for 50k guids —
because it stops ES decompressing and parsing 10,000 whole documents per page just to read one
field. `EsHit` carries both `_source` and `fields`, defaulted, so one envelope decodes a search
or an export.

**`track_total_hits: false` on export pages.** An export never reads `hits.total`, and counting
it up to the cap was being redone on every page. `EsHits.total` is `Option<Total>` so the one
envelope still decodes a search, which does read it.

**Force-merging the index to one segment is what removed the 18s.** The residual cost was never
query shape — everything below was measured and rejected — it was cold-cache disk I/O reading the
guid doc values scattered across segments. One segment makes that column contiguous, so a cold
read is sequential instead of random:

| 10k-page, fresh query term | before | after |
| --- | --- | --- |
| cold | ~2.5-3s | 1.5s |
| warm | ~0.5s | 0.37s |

50k guids now enumerate in ~3s. `ingest.py` force-merges after the bulk load; the index is an
immutable snapshot, which is the one situation where `max_num_segments = 1` has no downside.
Verify with `GET /arctos/_segments`.

### Things measured and rejected

- **Scroll API instead of `search_after`.** Identical, ~2.5s per 10k page. The cost is not query
  re-execution.
- **Sorting by `_doc` instead of `guid`.** No difference (0.64s vs 0.68s warm).
- **Reading the guid from `hits[].sort[0]` instead of `docvalue_fields`.** A wash. An early
  measurement suggested 18.3s → 2.3s, but running the comparison in the opposite order on a
  fresh query reversed the result: whichever runs *first* pays the cold reads. Beware this trap
  when benchmarking ES — always alternate the order and use fresh query terms.
- **Raising `index.max_result_window` to page 50k at a time.** Saves five round trips, but the
  round trips are not the cost; the per-document reads are, and there would be just as many.

**What is left is page cache.** At ~3s for 50k guids the ES half is still the larger of the two,
and the next lever is PIT + sliced `search_after` to read the slices in parallel. It was not taken:
slices return arbitrary subsets, so "the first 100,000 guids in guid order" would mean fetching up
to N×100k and merge-truncating. Not worth owning until 3s is the complaint.

---

## 6. All columns are `VARCHAR` on purpose (D-parquet-3)

`build_parquet.py` reads with `all_varchar = true`, and neither `sample_size = -1` nor
`store_rejects` is used any more.

- Elasticsearch does all the filtering; DuckDB only groups text columns at startup and copies
  rows back out as CSV. A detected type is re-rendered as text on the way out anyway, and that
  round trip is where leading zeros, `1.20` → `1.2`, reformatted dates and empty-vs-null get
  lost.
- `store_rejects = true` **silently drops** rows that fail to parse — a column sniffed as
  `BIGINT` that later meets `NA` sends that row to a rejects table nobody reads. With everything
  as text nothing can fail to parse, so there is no rejects table to swallow records.
- The JSON-bearing columns (`attributedetail`, `json_locality`, `media`, `related_record_cache`)
  stay text. DuckDB's `JSON` type would only pay off if we queried *into* them, and `json_extract`
  works on `VARCHAR` via implicit cast if that ever changes.

---

## 7. Why DuckDB at all, given §6

Types were never the reason. What it earns, all of which survives `all_varchar`:

- **Columnar projection** — the guid-only scan was 0.07s against 10s for all columns. A
  row-oriented store pays for every column on every read.
- **The startup queries** — `countries`, `states`, `guid_prefixes` and the 335k-row taxa table
  come out of `GROUP BY` plus `unnest(string_split(genus, ';'))`.
- **Partition pruning and the semi join** — §2.
- **`COPY … (FORMAT csv, COMPRESSION gzip)`** — row selection to gzipped CSV in one streamed
  statement, with RFC 4180 quoting we do not have to own. This is the strongest argument: the
  alternative is hand-assembling CSV from ES JSON, which `to_csv` in `routes/search.rs` already
  does for the small search path and which nobody wants to own at 154 columns.

The genuine alternative that would delete this layer is putting all columns in the ES `_source`
and exporting from a scroll. That costs index size roughly the size of the dump plus a slower
reindex, and it was not taken.

---

## 8. Open items

- **Row order is not stable across exports.** The parallel Parquet scan emits partitions in
  whatever order they finish, so two exports of the same query hold the same 50,335 rows in a
  different order (and gzip to slightly different sizes). Harmless today; an `ORDER BY guid` costs
  a sort of the whole result if anyone needs it deterministic.
- **Startup does eleven full passes** — eight rank scans plus three facet scans. Folding them
  into one scan is a pure code change and the cheapest remaining win.
- **Cache the schema** to a JSON file keyed on the dataset mtime; it is derived from an immutable
  snapshot and is rebuilt on every start today.
- **The export buffers the whole gzip** before sending the first byte (~30 MB at the row cap).
  Marked `ponytail:` in `export_csv_gz`.
