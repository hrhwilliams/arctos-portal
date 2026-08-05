# Note 04 -- Export performance

This note explains why `/api/download` is built the way it is. All measurements here used the 2026-03-09
snapshot (5,771,371 rows, 154 columns) against a local Elasticsearch instance and the Parquet dataset on
the same machine. The numbers are specific to that machine; the ratios are the point.

**Where it ended up.** A 50,335-row export across 44 collections (`genus:Myodes` plus
`genus:Clethrionomys`, the benchmark query) went from **60.1 seconds to about 3.5-4 seconds warm** -- about 1.2 seconds of DuckDB
and about 3 seconds of Elasticsearch. The path works like this: Elasticsearch answers *which* records match. DuckDB reads *what* they
are from the Parquet file and writes the gzip output.

Export time was 22.7 seconds (18 seconds Elasticsearch, 4.2 seconds DuckDB) until three changes landed. None of these changes altered the
architecture: force-merging the index to one segment (section 5), setting `track_total_hits: false` on export
pages (section 5), and compacting the Parquet files to one file per partition (section 3).

---

## 1. The single-file Parquet was the original problem

`/api/download` took about 10 seconds regardless of how much data it exported. Thirty rows cost the same as
thirty thousand. This pattern is the signature of a full scan.

| query on the 940 MB single file | time |
| ------------------------------- | ---- |
| `guid = 'x'` (one value)        | 0.43s |
| `guid IN (2)`                   | 8.6s |
| `guid IN (30)`                  | 9.7s |
| `guid_prefix = 'MSB:Mamm' AND guid IN (30)` | 9.3s |
| `count(*)` reading only the guid column | 0.07s |

**DuckDB pushes a single `=` condition into the Parquet scan, but it does not push a disjunction.** With one equality, the
scan skips non-matching rows before it decodes the other 153 columns. Once the filter becomes
`IN`, `OR`, or a semi join, which is exactly what an export needs, DuckDB materializes every row and
filters afterward. Two guids cost the same as two million guids.

The guid-only scan at 0.07 seconds proves that the file itself reads fast. The cost comes from decoding columns.

### What did not work

- **Sorting by guid.** This change shrank the file (214 MB to 108 MB on a subset; sorted data compresses
  well), but it did not fix the lookup. Row-group pruning does not apply to a disjunction, regardless of row
  order.
- **Bloom filters.** The file carries none (0 of 48 row groups carry one). This absence explains why the
  single `=` fast path uses zone maps rather than a probe, and why that fast path does not extend to lists.
- **Dropping compression.** An uncompressed file (3.2 GB) read in 1.06 seconds against a zstd-9 file (214 MB) at 1.07 seconds
  on identical data. Decompression is not on the critical path. Snappy compression was marginally slower.
  **Never spend disk space on this.**
- **A DuckDB table with an ART index on guid.** Point lookups work, but they scale with guid count:
  1 guid took 0.078s, 30 guids took 0.53s, 300 guids took 3.1s, about 10ms each. At that rate, 100,000 guids would take about 15
  minutes. This index also cost 4.6 GB for 481,000 rows, which extrapolates to about 56 GB.

---

## 2. Partitioning by `guid_prefix` (D-parquet-1)

The only lever that moves export time is **reading fewer rows**, and the cheapest filter is a filename.
`build_parquet.py` writes one directory per collection:

```
arctos_parquet/guid_prefix=MSB%3AMamm/data_0.parquet   (116 MB, 355k records)
arctos_parquet/guid_prefix=UAM%3AEnto/data_0.parquet   (90 MB)
...                                                     294 collections
```

The colon is URL-encoded. This encoding is what makes the path work on Windows. `hive_partitioning = true`
decodes the colon on read, so `WHERE guid_prefix = 'MSB:Mamm'` still matches. `guid_prefix` is **not**
stored inside the files. Every reader must pass this option, or the column is missing.

`export_csv_gz` derives the prefixes from the guids themselves (`MSB:Mamm:12345` carries its own
collection) and adds `WHERE p.guid_prefix IN (...)`. The same 30 guids, with and without that clause:

| | time |
| --- | ---- |
| pruned to one collection | 2.22s |
| no prefix filter | 11.35s |

Partition **size** barely matters once the query prunes to it. A 2 MB collection and a 122 MB collection land within
0.3 seconds of each other, because about 1.8 seconds of any export is fixed cost.

### What partitioning does not fix

Pruning selects whole collections and then **reads each matched collection in full**. A search
for a genus spans 40 or more mammal collections, so it approaches the cost of reading everything. That
cost is the design's floor, and it is why the column list (section 4) mattered more in the end.

---

## 3. Costs the layout introduced

**Fragmentation.** The writer emits a new file per partition on every flush. The first
build produced **7,945 files across 294 partitions** (about 27 files each). This layout cost about 0.95 seconds of glob and
footer reads on every query, against 0.18 seconds for a layout of about 300 files. The service also pays this cost eleven times
at startup, once per full-dataset pass (`build_taxa` runs one scan per rank, plus three facet
scans). This is why startup grew noticeably slower.

**Fixed.** `build_parquet.py` now runs a `compact()` pass after the partitioned write: one `COPY`
statement per fragmented partition, reading its own files back and rewriting a single `data_0.parquet` file. The service uses a
per-partition loop rather than a larger `partitioned_write_flush_threshold`, because a threshold
large enough for the 355,000-row largest partition would demand the same memory that caused an earlier OOM. This pass reads
*without* `hive_partitioning`, so the prefix stays in the directory name, and no stray column lands
in the file. `python build_parquet.py --compact [dir]` runs this pass standalone against an existing
dataset. It reads and rewrites the data; it does not re-parse the CSV source.

This pass reduced 8,056 files to 300, kept 5,771,371 rows unchanged, and ran for about 20 minutes. The benchmark export's DuckDB half went
from **4.2 seconds to 1.2 seconds** -- a larger gain than the roughly 0.8-second glob savings alone predicted, because the pass also reads the footers on the
matched partitions once each, rather than 27 times each.

**Memory during the write.** The default `partitioned_write_flush_threshold` buffers 524,288 rows
*per partition per thread*. Across about 300 collections of 154-column rows, this default caused an OOM at
15 GB. `build_parquet.py` sets this threshold to 100,000, with `partitioned_write_max_open_files = 25`. Raising
this threshold produces fewer files but uses more memory; that is the whole trade-off.

---

## 4. Columns are the dominant cost (D-parquet-2)

For 50,334 rows across 45 collections, holding everything else constant:

| | time |
| --- | ---- |
| find the rows (join only, no columns) | **2.2s** |
| + all 154 columns, to Parquet (no CSV formatting) | 40.2s |
| + all 154 columns, to csv.gz | 85.9s |
| + 7 columns, to csv.gz | **2.2s** |

Finding the rows costs almost nothing. Materializing columns accounts for nearly all the cost, and this cost is
roughly linear in column count. Scattered rows touch nearly every row group in the matched collections, so
the query reads all 154 columns whether or not the export needs them.

`EXPORT_COLUMNS` in `state.rs` is therefore deliberately short:

```
guid, scientific_name, country, state_prov, use_license_url, attributedetail, related_record_cache
```

This list is the **default**, not the limit. `?cols=` on the download endpoint accepts any columns of the dump,
checked against the schema's `columns` list (spec 03, section 2.11). The table above is the price list for
that choice: requesting all 154 columns costs what requesting all 154 columns has always cost.

Note that the download uses a **different** field set than the search page. `events`, `relations`,
`event_date_min`, and `event_date_max` come from the ETL for Elasticsearch, and they do not exist in
the dump. `related_record_cache` is the nearest flat equivalent to `relations`, not the same
value. Adding columns increases time proportionally. Weigh that cost if anyone asks for
the full record back.

**Output compression is not worth tuning.** The Parquet-zstd-to-CSV-gzip conversion is not a wasteful re-encode
that a different choice could avoid. The two formats compress unrelated data (binary column chunks versus row-oriented
text), so the pipeline must decode and re-encode the data regardless of codec choice. gzip on the output
is also a net *win*: writing 39 MB beat writing 693 MB uncompressed, and gzip is what a browser
or Excel opens without extra steps.

---

## 5. The Elasticsearch half

`export_query` in `translate.rs` reuses the search's bool query, with `search_after` in place of
`from` (because an export routinely runs past the 10,000-document result window), and it fetches nothing but
the guid.

**`_source: false` plus `docvalue_fields: ["guid"]` produced a real win** -- about 24 seconds down to about 18 seconds for 50,000 guids --
because it stops Elasticsearch from decompressing and parsing 10,000 whole documents per page just to read one
field. `EsHit` carries both `_source` and `fields`, each defaulted, so one struct decodes a search
response or an export response.

**`track_total_hits: false` on export pages.** An export never reads `hits.total`, but Elasticsearch was recounting
it, up to the cap, on every page. `EsHits.total` is `Option<Total>`, so the same struct still decodes a search
response, which does read this value.

**Force-merging the index to one segment removed the 18-second cost.** The remaining cost was never
query shape; every option below was measured and rejected. The cost was cold-cache disk I/O reading the
guid doc values scattered across segments. One segment makes that column contiguous, so a cold
read becomes sequential instead of random:

| 10k-page, fresh query term | before | after |
| --- | --- | --- |
| cold | ~2.5-3s | 1.5s |
| warm | ~0.5s | 0.37s |

50,000 guids now enumerate in about 3 seconds. `ingest.py` force-merges the index after the bulk load. The index is an
immutable snapshot, which is the one case where `max_num_segments = 1` carries no downside.
Verify with `GET /arctos/_segments`.

### Options measured and rejected

- **Scroll API instead of `search_after`.** Identical performance, about 2.5 seconds per 10,000-row page. The cost does not come from query
  re-execution.
- **Sorting by `_doc` instead of `guid`.** No measurable difference (0.64s versus 0.68s warm).
- **Reading the guid from `hits[].sort[0]` instead of from `docvalue_fields`.** A wash. An early
  measurement suggested a change from 18.3 seconds to 2.3 seconds, but running the comparison in the opposite order on a
  fresh query reversed the result. Whichever run happens *first* pays the cold-cache cost. Watch for this trap
  when you benchmark Elasticsearch: always alternate the run order, and use fresh query terms.
- **Raising `index.max_result_window` to page 50,000 rows at a time.** This change saves five round trips, but the
  round trips are not the cost. The per-document reads are the cost, and the same number of reads would still happen.

**What remains is page cache cost.** At about 3 seconds for 50,000 guids, the Elasticsearch half is still the larger of the two costs,
and the next lever is a point-in-time search with sliced `search_after`, reading the slices in parallel. This note has not adopted that change:
slices return arbitrary subsets, so retrieving "the first 100,000 guids in guid order" would mean fetching up
to N times 100,000 rows and merge-truncating the result. This change is not worth the added complexity until 3 seconds becomes the complaint.

---

## 6. All columns are `VARCHAR` on purpose (D-parquet-3)

`build_parquet.py` reads with `all_varchar = true`, and it no longer uses `sample_size = -1` or
`store_rejects`.

- Elasticsearch does all the filtering. DuckDB only groups text columns at startup and copies
  rows back out as CSV. A detected type gets re-rendered as text on the way out regardless, and that
  round trip is where leading zeros, `1.20` becoming `1.2`, reformatted dates, and empty-versus-null distinctions get
  lost.
- `store_rejects = true` **silently drops** rows that fail to parse. A column that DuckDB sniffs as
  `BIGINT`, which later meets an `NA` value, sends that row to a rejects table nobody reads. With every column
  as text, no value can fail to parse, so no rejects table can swallow records.
- The JSON-bearing columns (`attributedetail`, `json_locality`, `media`, `related_record_cache`)
  stay as text. DuckDB's `JSON` type would pay off only if a query reached *into* these columns, and `json_extract`
  works on `VARCHAR` through an implicit cast if that need ever arises.

---

## 7. Why DuckDB at all, given section 6

Types were never the reason to use DuckDB. Here is what DuckDB earns, all of which survives `all_varchar`:

- **Columnar projection** -- the guid-only scan took 0.07 seconds against 10 seconds for all columns. A
  row-oriented store pays the cost of every column on every read.
- **The startup queries** -- `countries`, `states`, `guid_prefixes`, and the 335,000-row taxa table
  come from `GROUP BY` plus `unnest(string_split(genus, ';'))`.
- **Partition pruning and the semi join** -- see section 2.
- **`COPY ... (FORMAT csv, COMPRESSION gzip)`** -- this statement streams row selection to gzipped CSV in one
  statement, with RFC 4180 quoting the service does not have to implement itself. This is the strongest argument for DuckDB: the
  alternative is hand-assembling CSV from Elasticsearch JSON, which `to_csv` in `routes/search.rs` already
  does for the small search path, and which nobody wants to own at 154 columns.

The real alternative that would remove this layer is putting all columns in the Elasticsearch `_source`
field and exporting from a scroll. That approach costs an index roughly the size of the dump, plus a slower
reindex step. This note does not adopt that approach.

---

## 8. Open items

- **Row order is not stable across exports.** The parallel Parquet scan emits partitions in
  whatever order they finish, so two exports of the same query hold the same 50,335 rows in a
  different order, and they gzip to slightly different sizes. This behavior is harmless today. An `ORDER BY guid` clause costs
  a full sort of the result if a caller ever needs deterministic order.
- **Startup runs eleven full passes** -- eight rank scans plus three facet scans. Folding them
  into one scan is a pure code change and the cheapest remaining win.
- **Cache the schema** to a JSON file keyed on the dataset's modification time. The schema is derived from an immutable
  snapshot, and the service rebuilds it on every start today.
- **The export buffers the whole gzip file** before it sends the first byte (about 30 MB at the row cap).
  Marked `ponytail:` in `export_csv_gz`.
