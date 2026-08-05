# Spec 03 — Schema API

`GET /api/schema` on the Rust service. Depends on spec 01 (index/mapping) and spec 09 (code tables).

**Status.** The frontend does not call this endpoint yet. It imports a generated fixture,
`src/lib/fixtures/schema.json` (314 KB), built by `tools/build_fixtures.py` from the real
Arctos code tables in `docs/data/code-tables/` plus the dump profiles in `docs/data/profiles/`.
This document is the contract the backend must satisfy so that import can be deleted.

**The fixture is the specification.** Every field below is already consumed by real component
code. Byte-for-byte, `GET /api/schema` must return what `build_fixtures.py` writes, minus
`fixture`, with the counts and vocabularies computed from the live snapshot rather than the
199-row sample. If the backend response deserialises into `src/lib/types.ts`'s `Schema` type and
the form renders identically against it, the work is done.

An earlier revision of this spec described `outcomes`, `analysis_values`, `countries` as bare
strings, and a `methods` list. All of that is superseded — the form is now built from
`attribute_types` + `vocabularies` (D36, spec 09), which covers the four detection types as
ordinary attribute types. See git history for the old text.

---

## 1. Response

```jsonc
{
  "snapshot_date": "2026-03-09",
  "ranks":            [ /* §2.2 */ ],
  "attribute_types":  [ /* §2.3 */ ],
  "vocabularies":     { /* §2.4 */ },
  "countries":        [ /* §2.5 */ ],
  "states":           [ /* §2.5 */ ],
  "relations":        [ /* §2.6 */ ],
  "guid_prefixes":    [ /* §2.7 */ ],
  "sorts":            [ /* §2.8 */ ],
  "limits":           { /* §2.9 */ },
  "nonpublic_types_dropped": [ /* §2.10 */ ]
}
```

All eleven keys are **required and non-null**. Empty is allowed (`[]`, `{}`); absent is not —
the client destructures without guards, and a missing `relations` is a render crash, not a
degraded form. `fixture: true` is the one key the service must **not** send; it exists so the
homepage can say the data is fake, and its absence is how the frontend knows it is talking to a
real service.

Sizes in the current fixture, as an order-of-magnitude target: 8 ranks, 142 attribute types,
14 vocabularies / 714 values, 117 countries, 488 states, 23 relations, 74 guid prefixes.
Against the full dump, expect countries/states/prefixes to grow and everything else to stay put.

### 1.1 Sources at a glance

| Field                     | Source                                            | Complete list? |
| ------------------------- | ------------------------------------------------- | -------------- |
| `snapshot_date`           | ETL metadata doc (D41, §5)                        | —              |
| `ranks`                   | static list, filtered by `_field_caps` (§2.2)     | yes            |
| `attribute_types`         | `ctattribute_type`, `public != 0` only            | yes            |
| `vocabularies`            | code tables named by `value_code_table`            | yes            |
| `countries` / `states`    | `terms` aggregation on the index                  | no — observed  |
| `relations`               | `ctid_references`                                 | yes            |
| `guid_prefixes`           | `terms` aggregation + `ctcollection_cde` for label | no — observed  |
| `sorts` / `limits`        | service constants                                 | yes            |
| `nonpublic_types_dropped` | `ctattribute_type`, `public == 0` only            | yes            |

The split matters: **code-table-sourced lists are complete and carry no counts; aggregation-sourced
lists are observed and must carry counts.** A controlled value with zero records must still appear
(D36) — an option that vanishes between snapshots silently changes the meaning of a saved URL.
A place name with zero records cannot appear, because nothing enumerates it.

---

## 2. Fields

### 2.1 `snapshot_date` — string, `YYYY-MM-DD`

Rendered verbatim as "Data current as of {date}" at [+page.svelte:115](../../src/routes/search/+page.svelte#L115).
Date only, no time, no timezone. Source is the ETL-written metadata document (§5), not the index
creation date.

### 2.2 `ranks` — array

```json
[
  { "id": "scientific_name", "label": "Scientific name", "field": "scientific_name" },
  { "id": "phylum",    "label": "Phylum",    "field": "phylum" },
  { "id": "class",     "label": "Class",     "field": "phylclass" },
  { "id": "order",     "label": "Order",     "field": "phylorder" },
  { "id": "family",    "label": "Family",    "field": "family" },
  { "id": "subfamily", "label": "Subfamily", "field": "subfamily" },
  { "id": "genus",     "label": "Genus",     "field": "genus" },
  { "id": "species",   "label": "Species",   "field": "species" }
]
```

- Populates the rank `<select>` on every taxon row ([TaxonRows.svelte:39](../../src/lib/components/TaxonRows.svelte#L39)).
- `id` is what the client sends back in `taxon=<rank>|<name>`; spec 02 validates against this list.
- `label` is display text. `field` is the index field — sent so the label→field mapping lives in
  one place; the client stores it but does not currently use it, and it must still be present.
- **Order is significant.** Rendered in array order, and `scientific_name` must be first: it is
  not a rank (it is a Latin binomial matched as a phrase, see `SCIENTIFIC_NAME` in `types.ts`)
  but it is the field users reach for first.
- Omit a rank the current index has no values for, determined once at startup via `_field_caps`
  plus a cheap non-zero check. `scientific_name` is never omitted.

### 2.3 `attribute_types` — array

```json
{
  "id": "abundance",
  "label": "abundance",
  "description": "A subjective description of the abundance of organisms, conspecific with the cataloged item, at the collecting locality.",
  "vocabulary": null,
  "units_table": null
}
```

Every public row of `ctattribute_type`, mapped:

| Key           | From                       | Notes                                                        |
| ------------- | -------------------------- | ------------------------------------------------------------ |
| `id`          | `attribute_type`           | Sent back as `attr=<id>\|<value>`; must match the index value |
| `label`       | `attribute_type`           | Same string today; kept separate so it can diverge            |
| `description` | `description`, trimmed     | `""` when absent, never null                                  |
| `vocabulary`  | `value_code_table`         | Table **name**, or `null`. Must be a key of `vocabularies`    |
| `units_table` | `unit_code_table`          | Table name, or `null`. See below                              |

- **Sort by `id.lower()` ascending.** It is a 142-item flat `<select>` with no filter.
- **`public == 0` rows must never appear here** (D35, spec 09) — currently `NAGPRA category`,
  `restricted data`, `value`. They are also dropped in the ETL, so this is the second of two
  gates, not the only one.
- `vocabulary` drives the value control at [AttributeRows.svelte:71](../../src/lib/components/AttributeRows.svelte#L71):
  non-null → multi-select of that vocabulary, null → free-text input. A non-null `vocabulary`
  naming a table absent from `vocabularies` silently degrades to free text — treat that as a
  startup error, not a runtime shrug.
- `units_table` is carried and currently unused (65 of 142 types are measurements referencing
  six unit tables). The measurement filters are post-v1; ship the field now so the schema does
  not need a version bump later. The unit tables themselves need **not** be present in
  `vocabularies` until those filters exist.
- 22 of 142 types have a `vocabulary`. The four detection types (`detected`, `not detected`,
  `examined for`, `not examined for`) are ordinary rows here, all four pointing at
  `ctexamined_detected` — the form has no special-casing for them and needs none.

### 2.4 `vocabularies` — object, keyed by code-table name

```json
"ctexamined_detected": [
  { "value": "DNA",        "description": "Deoxyribonucleic acid…", "documentation_url": "" },
  { "value": "bacteria",   "description": "…", "documentation_url": "https://handbook.arctosdb.org/…" },
  { "value": "bacteria: Francisella tularensis", "description": "…", "documentation_url": "" }
]
```

- **Include exactly the tables named by a public `attribute_type.value_code_table`** — 14 today.
  Not all 130 fetched tables. `ctid_references` is *not* here; it ships as `relations` (§2.6).
- Value column: each code table names its value column differently (`attribute_type`,
  `examined_detected`, `sex_cde`, …). `build_fixtures.py` takes the first key that is not row
  metadata; the backend can do the same or hardcode the mapping. `NON_VALUE_KEYS` in that script
  is the current exclusion list.
- `description` and `documentation_url` are `""` when absent, **never null**. `documentation_url`
  is a string even where Arctos returns an array — take the first element.
- **Sort: colon-depth ascending, then value case-insensitively ascending.** Parents precede
  children (`ectoparasite` before `ectoparasite: flea`), which is what lets the UI indent the
  hierarchy and matches the `path_hierarchy` expansion in spec 01. Max depth is 2 and the code
  table guarantees it (spec 09).
- Drop rows with an empty value.
- **No counts here today.** If per-value counts are added later (a `terms` agg on the bare
  keyword field, never `.hierarchy`, which returns synthetic ancestor tokens), add an optional
  `count` — the client ignores unknown keys. Zero-count values stay in the list and get styled
  as disabled; they are not omitted.

### 2.5 `countries`, `states` — arrays of `{ value, count }`

```json
{ "value": "United States", "count": 600439 }
```

- `terms` aggregations on `country` and `state_prov`. No code table exists for either.
- **Sort by `count` descending.** Both render in multi-selects with the count as the option hint
  ([+page.svelte:73-77](../../src/routes/search/+page.svelte#L73-L77)); the list is filterable, so
  most-used-first beats alphabetical.
- `count` is an integer and is rendered with `toLocaleString()`. It must be present even if 0.
- `states` is flat and **not** scoped by country — 488 entries mixing "New Mexico" and
  "Chihuahua". If it is ever nested per country, that is a schema change with a form change
  behind it; do not do it silently.
- Aggregation size must be large enough to be complete. `size: 0` defaults will truncate; use an
  explicit high `size` and check `sum_other_doc_count == 0` at startup, log if not.

### 2.6 `relations` — array of `{ value, description }`

```json
{ "value": "associated with", "description": "The cataloged item was or is physically in contact with…" }
```

- From `ctid_references`, the whole table. Sorted by `value` case-insensitively ascending.
- **No counts, deliberately** — it is a code table, so every relationship exists whether or not
  this snapshot uses it ([TaxonRows.svelte:9-12](../../src/lib/components/TaxonRows.svelte#L9-L12)).
- These are `relations.relationship` in the mapping: how a record relates to *another cataloged
  item* (`parasite of`, `host of parasite`). Not taxon-name relationships — `cttaxon_relation`
  holds `synonym of` / `misspelling` and is a different thing (spec 09).
- Spec 08 derived 16 relationship types empirically and the code table lists 23. **Serve the
  code table's list**, and log — never reject — a value found in the data that the table does
  not document.

### 2.7 `guid_prefixes` — array

```json
{ "value": "MSB:Mamm", "count": 355691, "institution": "MSB", "collection_cde": "Mammalogy" }
```

- `terms` aggregation on `guid_prefix`, **sorted by `count` descending**.
- `institution` and `collection_cde` are the prefix split on `:`, with the collection code
  expanded to its label via `ctcollection_cde` (`Mamm` → `Mammalogy`). Both are `""` when
  unresolvable, never null.
- The form currently labels options with `value` and hints with `count`; institution and
  collection are carried for the grouped picker in spec 04. Ship them.

### 2.8 `sorts` — array of `{ id, label }`

```json
[
  { "id": "guid_asc",  "label": "Catalog number" },
  { "id": "date_desc", "label": "Newest collection date" },
  { "id": "date_asc",  "label": "Oldest collection date" }
]
```

- The client sends `sort=<id>`; `guid_asc` is the default and is omitted from the URL when
  selected, so **`guid_asc` must always be a valid id** and should stay first.
- The form has no sort control yet (`SearchQuery.sort` is populated from the URL and defaults to
  `guid_asc`). The list must still be served — the control is a small frontend change that
  should not need a backend one.

### 2.9 `limits` — object

```json
{ "max_per_page": 200, "max_result_window": 10000, "max_export_rows": 100000 }
```

Present in the fixture, **absent from the `Schema` TypeScript type**, and the page hardcodes the
numbers instead ([+page.svelte:91-93](../../src/routes/search/+page.svelte#L91-L93)):

```ts
const PAGE_SIZE = 100;          // "the service fixes the page size and takes no per_page"
const MAX_RESULT_WINDOW = 10000; // "service-side value; change both together"
```

That is a duplicated constant in two languages and it should die with the fixture. Two requirements:

1. Serve `limits` with the values the service actually enforces, so spec 02 rejecting a request
   and the client disabling the button agree by construction.
2. **Add the page size.** The service fixes it and takes no `per_page`, so `max_per_page: 200` is
   not the number the pager needs — it needs the *actual* page size. Serve
   `"page_size": 100` alongside the existing keys. Without it the pager's arithmetic
   (`ceil(matched / PAGE_SIZE)`) stays a hardcoded guess about backend behaviour.

Client use: `pageCount = ceil(matched / page_size)`, browsing stops at
`floor(max_result_window / page_size)`, and export is not capped by the window.

### 2.10 `nonpublic_types_dropped` — array of strings

```json
["NAGPRA category", "restricted data", "value"]
```

The `public == 0` attribute types the ETL excluded (D35). Not rendered anywhere today; it exists
so "we filtered these" is an assertion the running service makes rather than a claim in a build
log. Serve it; a provenance panel is the intended consumer.

---

## 3. Transport

| Concern           | Requirement                                                                          |
| ----------------- | ------------------------------------------------------------------------------------ |
| Method / path     | `GET /api/schema`, no parameters. Unknown params ignored, not 400                     |
| Auth              | None — public, unauthenticated, like the rest (D15/D38)                               |
| CORS              | Must allow the portal origin. `load` runs in the browser on client-side navigation    |
| Encoding          | `Content-Type: application/json; charset=utf-8`. Values carry non-ASCII place names   |
| Compression       | gzip/br **required** — ~300 KB raw, ~40 KB compressed. Do not ship this uncompressed  |
| Caching           | `Cache-Control: public, max-age=900` and a strong `ETag` keyed on the snapshot        |
| Latency budget    | Served from memory. It blocks the first render of the search page                     |
| Rate limit        | Exempt or generous; one call per page load, and CDN-cacheable                         |

Compute the whole document at startup into `Arc<RwLock<Schema>>`. Refresh on a 15-minute
interval and force a refresh when the alias target changes. **If a refresh fails, keep serving
the stale copy and log** — a schema refresh failure must never take the form down.

Code tables are read from the snapshot on disk written by `tools/fetch_code_tables.py`, **not**
fetched from Arctos at request time. That API is IP-bound and third-party; a public portal must
not fail to render its form because someone else's service is down.

### Failure

There is no partial schema. If the service cannot build one, `GET /api/schema` returns
`503` with `{ "error": "...", "message": "..." }` — the same envelope as `/api/search`, ES
detail never forwarded. The frontend renders the "cannot reach the search service" state; it
does not fall back to a fixture in production.

---

## 4. `GET /api/taxa` — the other fixture

`src/lib/fixtures/taxa.json` (249 KB, 1,835 rows) is imported directly by
[TaxonCombobox.svelte:3](../../src/lib/components/TaxonCombobox.svelte#L3), which filters it
client-side. It is a second hardcoded table and it goes away in the same piece of work.

**The contract is spec 10 and it needs no changes.** `GET /api/taxa?rank=genus&q=sor&limit=10`
→ `{ "matches": [ { rank, name, record_count, parent_rank, parent_name } ] }`, sorted by
`record_count` desc then name asc, `q` ≥ 2 chars enforced server-side, `limit` capped at 20,
`Cache-Control: public, max-age=3600`, 120 req/min per IP.

Two notes from the frontend that the backend must honour:

- The component already implements spec 10's client behaviour — prefix match on the whole name,
  ≥ 2 characters, rank-filtered, capped at 10 — and it **drops an exact match** on the grounds
  that suggesting back what is already typed is noise. That filtering stays client-side of the
  response; the backend does not need to replicate it.
- `parent_rank` / `parent_name` are `null` for `scientific_name` rows and non-null elsewhere.
  The UI shows only name and count today, but spec 10's homonym disambiguation
  (`Arvicolinae (subfamily, in Cricetidae)`) depends on them. Serve them.

The combobox debounce must go to 200 ms with in-flight requests cancelled (spec 10) when it
starts hitting the network — it has none today, because a local array needs none.

---

## 5. Where `snapshot_date` comes from

D2 means the data is stale by design, so the UI says so. The source is an ETL-written metadata
document, one per index (D41):

```json
{
  "doc_type": "snapshot_metadata",
  "snapshot_date": "2026-07-01",
  "extracted_at": "2026-07-01T04:12:00Z",
  "source_file": "arctos-export-20260701.csv",
  "row_count": 10482913,
  "indexed_count": 10482913,
  "code_tables_fetched": "2026-07-01",
  "assertions": {
    "encumbrances_empty": true,
    "nonpublic_attributes_dropped": 0,
    "undocumented_attribute_types": []
  }
}
```

The alias target's creation date was the alternative and is rejected: it records when the index
was *built*, not when the data was *extracted*, and those differ by however long the ETL queue
was. `extracted_at` depends on the dump carrying a timestamp (request 4, spec 11); until that
lands it equals the ETL run time, which is why the UI shows the date only.

Carrying the D29/D35 assertion results in that document is what lets `/api/schema` eventually
surface "this snapshot was verified" instead of the guarantee living in a build log nobody reads.

---

## 6. Frontend changes this unblocks

Small, and listed so the backend knows what is waiting on it:

1. `src/routes/search/+page.ts` — replace the `schemaFixture` import with a fetch of
   `${__API_BASE__}/api/schema`, alongside the search fetch it already does. One request per
   page load; SvelteKit's `load` dedupes across client-side navigation.
2. `TaxonCombobox.svelte` — replace the `taxa.json` import with a debounced, cancellable fetch
   of `/api/taxa`.
3. `src/lib/types.ts` — add `limits` (including `page_size`) to `Schema`; delete `PAGE_SIZE` and
   `MAX_RESULT_WINDOW` from `+page.svelte`.
4. Delete `src/lib/fixtures/` and drop the fixture note from the homepage. `build_fixtures.py`
   stays — it is the reference implementation of this contract and the e2e tests' data source.

`DETECTION_TYPES` and `SCIENTIFIC_NAME` in `types.ts` stay hardcoded on purpose: the first is a
query-shape mapping (which index field a detection type writes to), the second is a sentinel for
a non-rank. Neither is vocabulary, and neither belongs in `/api/schema`.

---

## 7. Tests the backend owes

- Startup against an **empty index** yields a schema with empty aggregation lists, full
  code-table lists, and does not panic.
- A refresh failure preserves the previous schema and logs.
- `ranks` omits a rank with no values in the index; `scientific_name` survives.
- No `public: 0` attribute type appears in `attribute_types`; all three appear in
  `nonpublic_types_dropped`.
- Every non-null `attribute_types[].vocabulary` is a key of `vocabularies`.
- `vocabularies.ctexamined_detected` has all 32 values, `ectoparasite` sorts before
  `ectoparasite: flea`, and a value with zero records is present rather than omitted.
- `countries` / `states` / `guid_prefixes` are count-descending and their aggregations report
  `sum_other_doc_count == 0`.
- The response deserialises into the fixture's shape — assert against a checked-in golden built
  by `tools/build_fixtures.py`, so a shape drift fails the backend build rather than the form.
