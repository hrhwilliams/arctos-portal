# Spec 03 -- Schema API

`GET /api/schema` runs on the Rust service. This spec depends on spec 01 (index/mapping) and spec 09 (code tables).

**Status.** The frontend does not call this endpoint yet. The frontend imports a generated fixture,
`src/lib/fixtures/schema.json` (314 KB). `tools/build_fixtures.py` builds this fixture from the real
Arctos code tables in `docs/data/code-tables/` plus the dump profiles in `docs/data/profiles/`.
This document is the contract the backend must satisfy. Once the backend satisfies it, the import can be deleted.

**The fixture is the specification.** Real component code already consumes every field below.
`GET /api/schema` must return, byte-for-byte, what `build_fixtures.py` writes, minus the
`fixture` key. The response must compute counts and vocabularies from the live snapshot, not from the
199-row sample. If the backend response deserializes into `src/lib/types.ts`'s `Schema` type, and
the form renders the same way against it, the work is done.

An earlier revision of this spec described `outcomes`, `analysis_values`, and `countries` as bare
strings, and it described a `methods` list. That earlier text is superseded. The form is now built from
`attribute_types` plus `vocabularies` (D36, spec 09). This design covers the four detection types as
ordinary attribute types. See git history for the old text.

---

## 1. Response

```jsonc
{
  "snapshot_date": "2026-03-09",
  "columns":          [ /* section 2.11 */ ],
  "ranks":            [ /* section 2.2 */ ],
  "attribute_types":  [ /* section 2.3 */ ],
  "vocabularies":     { /* section 2.4 */ },
  "countries":        [ /* section 2.5 */ ],
  "states":           [ /* section 2.5 */ ],
  "relations":        [ /* section 2.6 */ ],
  "guid_prefixes":    [ /* section 2.7 */ ],
  "sorts":            [ /* section 2.8 */ ],
  "limits":           { /* section 2.9 */ },
  "nonpublic_types_dropped": [ /* section 2.10 */ ]
}
```

All twelve keys are **required and non-null**. An empty value is allowed (`[]`, `{}`). A missing key is not
allowed. The client destructures the response without guards. A missing `relations` key causes a render crash,
not a degraded form. `fixture: true` is the one key the service must **not** send. This key exists so the
homepage can say the data is fake. Its absence tells the frontend that it is talking to a
real service.

Sizes in the current fixture, as an order-of-magnitude target: 8 ranks, 142 attribute types,
14 vocabularies with 714 values, 117 countries, 488 states, 23 relations, 74 guid prefixes.
Against the full dump, expect the countries, states, and prefixes counts to grow. Expect the other counts to stay the same.

### 1.1 Sources at a glance

| Field                     | Source                                            | Complete list? |
| ------------------------- | ------------------------------------------------- | -------------- |
| `snapshot_date`           | ETL metadata doc (D41, section 5)                 | --              |
| `columns`                 | `DESCRIBE` on the Parquet (section 2.11)          | yes            |
| `ranks`                   | static list, filtered by `_field_caps` (section 2.2) | yes         |
| `attribute_types`         | `ctattribute_type`, `public != 0` only            | yes            |
| `vocabularies`            | code tables named by `value_code_table`            | yes            |
| `countries` / `states`    | `terms` aggregation on the index                  | no -- observed  |
| `relations`               | `ctid_references`                                 | yes            |
| `guid_prefixes`           | `terms` aggregation + `ctcollection_cde` for label | no -- observed  |
| `sorts` / `limits`        | service constants                                 | yes            |
| `nonpublic_types_dropped` | `ctattribute_type`, `public == 0` only            | yes            |

The split matters. **A code-table-sourced list is complete and carries no counts. An aggregation-sourced
list is observed and must carry counts.** A controlled value with zero records must still appear
(D36). An option that vanishes between snapshots silently changes the meaning of a saved URL.
A place name with zero records cannot appear, because nothing enumerates it.

---

## 2. Fields

### 2.1 `snapshot_date` -- string, `YYYY-MM-DD`

The frontend renders this value verbatim as "Data current as of {date}" at [+page.svelte:115](../../src/routes/search/+page.svelte#L115).
This field holds a date only. It carries no time and no timezone. The source is the ETL-written metadata document (section 5), not the index
creation date.

### 2.2 `ranks` -- array

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

- This list populates the rank `<select>` on every taxon row ([TaxonRows.svelte:39](../../src/lib/components/TaxonRows.svelte#L39)).
- `id` is the value the client sends back in `taxon=<rank>|<name>`. Spec 02 validates a request against this list.
- `label` is display text. `field` is the index field. The service sends `field` so the label-to-field mapping lives in
  one place. The client stores `field` but does not currently use it. The client must still receive it.
- **Array order matters.** The form renders ranks in array order. `scientific_name` must sort first. It is
  not a rank; it is a Latin binomial matched as a phrase (see `SCIENTIFIC_NAME` in `types.ts`).
  Users still reach for it first.
- The service omits a rank when the current index has no values for it. The service determines this once at startup, using `_field_caps`
  plus a cheap non-zero check. The service never omits `scientific_name`.

### 2.3 `attribute_types` -- array

```json
{
  "id": "abundance",
  "label": "abundance",
  "description": "A subjective description of the abundance of organisms, conspecific with the cataloged item, at the collecting locality.",
  "vocabulary": null,
  "units_table": null
}
```

The service maps every public row of `ctattribute_type` as follows:

| Key           | From                       | Notes                                                        |
| ------------- | --------------------------- | ------------------------------------------------------------ |
| `id`          | `attribute_type`           | Sent back as `attr=<id>\|<value>`. Must match the index value |
| `label`       | `attribute_type`           | Same string today. Kept as a separate key so it can diverge   |
| `description` | `description`, trimmed     | `""` when absent, never null                                  |
| `vocabulary`  | `value_code_table`         | Table **name**, or `null`. Must be a key of `vocabularies`    |
| `units_table` | `unit_code_table`          | Table name, or `null`. See below                              |

- **The service sorts this list by `id.lower()`, ascending.** It is a 142-item flat `<select>` with no filter.
- **A `public == 0` row must never appear here** (D35, spec 09). Today this rule excludes `NAGPRA category`,
  `restricted data`, and `value`. The ETL also drops these rows. This check is the second of two
  gates, not the only gate.
- `vocabulary` drives the value control at [AttributeRows.svelte:71](../../src/lib/components/AttributeRows.svelte#L71).
  A non-null value renders a multi-select of that vocabulary. A null value renders a free-text input.
  A non-null `vocabulary` that names a table absent from `vocabularies` degrades silently to free text.
  Treat that case as a startup error, not a runtime event to ignore.
- `units_table` is carried in the response but currently unused. 65 of 142 types are measurements that reference
  six unit tables. The measurement filters ship after v1. The service ships this field now so the schema does
  not need a version bump later. The unit tables themselves need **not** appear in
  `vocabularies` until those filters exist.
- 22 of 142 types have a `vocabulary`. The four detection types (`detected`, `not detected`,
  `examined for`, `not examined for`) are ordinary rows here. All four point at
  `ctexamined_detected`. The form needs no special case for them.

### 2.4 `vocabularies` -- object, keyed by code-table name

```json
"ctexamined_detected": [
  { "value": "DNA",        "description": "Deoxyribonucleic acid...", "documentation_url": "" },
  { "value": "bacteria",   "description": "...", "documentation_url": "https://handbook.arctosdb.org/..." },
  { "value": "bacteria: Francisella tularensis", "description": "...", "documentation_url": "" }
]
```

- **Include exactly the tables that a public `attribute_type.value_code_table` names** -- 14 tables today.
  Do not include all 130 fetched tables. `ctid_references` does **not** belong here. It ships as `relations` (section 2.6).
- Value column: each code table names its value column differently (`attribute_type`,
  `examined_detected`, `sex_cde`, and others). `build_fixtures.py` takes the first key that is not row
  metadata. The backend can use the same rule or a hardcoded mapping. `NON_VALUE_KEYS` in that script
  is the current exclusion list.
- `description` and `documentation_url` are `""` when absent, **never null**. `documentation_url`
  is a string even where Arctos returns an array. Take the first array element.
- **Sort order: colon-depth ascending, then value case-insensitively ascending.** A parent value precedes its
  child values (`ectoparasite` before `ectoparasite: flea`). This order lets the UI indent the
  hierarchy, and it matches the `path_hierarchy` expansion in spec 01. The code table caps the depth at 2.
- Drop rows with an empty value.
- **This response carries no counts today.** If per-value counts are added later (a `terms` aggregation on the bare
  keyword field, never `.hierarchy`, which returns synthetic ancestor tokens), add an optional
  `count` field. The client ignores unknown keys. A zero-count value stays in the list and gets styled
  as disabled. The service must not omit it.

### 2.5 `countries`, `states` -- arrays of `{ value, count }`

```json
{ "value": "United States", "count": 600439 }
```

- These fields come from `terms` aggregations on `country` and `state_prov`. No code table exists for either field.
- **Sort by `count`, descending.** Both fields render in multi-selects with the count as the option hint
  ([+page.svelte:73-77](../../src/routes/search/+page.svelte#L73-L77)). The list is filterable, so
  most-used-first serves users better than alphabetical order.
- `count` is an integer. The client renders it with `toLocaleString()`. The field must be present even when the count is 0.
- `states` is a flat list, **not** scoped by country. It has 488 entries that mix "New Mexico" and
  "Chihuahua". Nesting `states` per country later is a schema change with a form change
  behind it. Do not make that change silently.
- The aggregation size must be large enough to return a complete list. `size: 0` defaults truncate the list; use an
  explicit high `size` value, and check `sum_other_doc_count == 0` at startup. Log a warning if the check fails.

### 2.6 `relations` -- array of `{ value, description }`

```json
{ "value": "associated with", "description": "The cataloged item was or is physically in contact with..." }
```

- This field comes from `ctid_references`, the whole table. The service sorts it by `value`, case-insensitively, ascending.
- **This field deliberately carries no counts.** It is a code table, so every relationship exists whether or not
  the current snapshot uses it ([TaxonRows.svelte:9-12](../../src/lib/components/TaxonRows.svelte#L9-L12)).
- These values are `relations.relationship` in the mapping. They describe how a record relates to *another cataloged
  item* (`parasite of`, `host of parasite`). They are not taxon-name relationships. `cttaxon_relation`
  holds `synonym of` and `misspelling`, a different concept (spec 09).
- Spec 08 derived 16 relationship types empirically. The code table lists 23. **Serve the
  code table's list.** Log, and never reject, a value found in the data that the table does
  not document.

### 2.7 `guid_prefixes` -- array

```json
{ "value": "MSB:Mamm", "count": 355691, "institution": "MSB", "collection_cde": "Mammalogy" }
```

- This field comes from a `terms` aggregation on `guid_prefix`, **sorted by `count`, descending**.
- `institution` and `collection_cde` are the prefix, split on `:`, with the collection code
  expanded to its label through `ctcollection_cde` (`Mamm` maps to `Mammalogy`). Both fields are `""` when
  unresolvable, never null.
- The form currently labels each option with `value` and hints with `count`. `institution` and
  `collection_cde` support the grouped picker planned in spec 04. Ship them now.

### 2.8 `sorts` -- array of `{ id, label }`

```json
[
  { "id": "guid_asc",  "label": "Catalog number" },
  { "id": "date_desc", "label": "Newest collection date" },
  { "id": "date_asc",  "label": "Oldest collection date" }
]
```

- The client sends `sort=<id>`. `guid_asc` is the default value, and the client omits it from the URL when
  selected. **`guid_asc` must always be a valid id** and should sort first.
- The form has no sort control yet. `SearchQuery.sort` is populated from the URL and defaults to
  `guid_asc`. The service must still serve this list. Adding the control is a small frontend change that
  should not require a backend change.

### 2.9 `limits` -- object

```json
{ "max_per_page": 200, "max_result_window": 10000, "max_export_rows": 100000 }
```

This object exists in the fixture. It is **absent from the `Schema` TypeScript type**. The page hardcodes these
numbers instead ([+page.svelte:91-93](../../src/routes/search/+page.svelte#L91-L93)):

```ts
const PAGE_SIZE = 100;          // "the service fixes the page size and takes no per_page"
const MAX_RESULT_WINDOW = 10000; // "service-side value; change both together"
```

This code duplicates a constant in two languages, and it should be deleted along with the fixture. Two requirements follow:

1. Serve `limits` with the values the service actually enforces. This step ensures that spec 02's request rejection
   and the client's button-disabling logic agree by construction.
2. **Add the page size.** The service fixes the page size and accepts no `per_page` value, so `max_per_page: 200` is
   not the number the pager needs; it needs the *actual* page size. Serve
   `"page_size": 100` alongside the existing keys. Without this field, the pager's arithmetic
   (`ceil(matched / PAGE_SIZE)`) stays a hardcoded guess about backend behavior.

Client use: `pageCount = ceil(matched / page_size)`. Browsing stops at
`floor(max_result_window / page_size)`. Export is not capped by the window.

### 2.10 `nonpublic_types_dropped` -- array of strings

```json
["NAGPRA category", "restricted data", "value"]
```

This list holds the `public == 0` attribute types the ETL excluded (D35). The frontend does not render it today. This field lets
the running service assert "we filtered these types" instead of leaving that claim in a build
log. Serve it; a provenance panel is the intended consumer.

### 2.11 `columns` -- array of strings

```json
["cataloged_item_type", "lastdate", "lastuser", "collection_object_id", "..."]
```

This list holds every column of the dump, in dump order -- 154 columns in the 2026-03-09 snapshot, plus
`guid_prefix`, which lives in the directory name rather than in the files. The service reads this list at startup with
`DESCRIBE SELECT * FROM <parquet> LIMIT 0`. This method derives the list from the snapshot, so the list cannot drift
from it.

This list serves two consumers:

- **The client** uses it to offer a column picker for the download.
- **The service** uses it as the allowlist that checks `GET /api/download?cols=`. A requested name
  must match a list entry exactly, case-sensitively, with no globs. The SQL query uses the
  *allowlist's* copy of the string, never the caller's copy. An unknown name returns a `400` error naming it. This check is the whole of the input handling on that parameter.

`?cols=` takes a comma-separated list. An absent or blank value means the default export set (see note 04, section 4).
**Adding columns increases export time roughly linearly** -- 7 columns costs about 2 seconds where all 154 columns cost about 86 seconds at
50,000 rows. A column picker should show users that selecting everything is slow. Do not present it as free.

---

## 3. Transport

| Concern           | Requirement                                                                          |
| ----------------- | ------------------------------------------------------------------------------------ |
| Method / path     | `GET /api/schema`, no parameters. The service ignores unknown params; it does not return 400 |
| Auth              | None. This endpoint is public and unauthenticated, like the rest of the API (D15/D38) |
| CORS              | Must allow the portal origin. `load` runs in the browser on client-side navigation    |
| Encoding          | `Content-Type: application/json; charset=utf-8`. Values carry non-ASCII place names   |
| Compression       | gzip/br **required** -- about 300 KB raw, about 40 KB compressed. Do not ship this response uncompressed |
| Caching           | `Cache-Control: public, max-age=900` and a strong `ETag` keyed on the snapshot        |
| Latency budget    | The service serves this response from memory. It blocks the first render of the search page |
| Rate limit        | Exempt or generous. The page makes one call per load, and the response is CDN-cacheable |

The service computes the whole document at startup into `Arc<RwLock<Schema>>`. It refreshes this document on a 15-minute
interval, and it forces a refresh when the alias target changes. **If a refresh fails, the service must keep serving
the stale copy and log the failure.** A schema refresh failure must never take the service down.

The service reads code tables from the snapshot on disk that `tools/fetch_code_tables.py` writes. The service does **not**
fetch code tables from Arctos at request time. That API is IP-bound and belongs to a third party. A public portal must
not fail to render its form because another organization's service is down.

### Failure

There is no partial schema. If the service cannot build one, `GET /api/schema` returns
`503` with `{ "error": "...", "message": "..." }`, the same envelope as `/api/search`. The service never forwards
Elasticsearch error detail. The frontend renders the "cannot reach the search service" state. It
does not fall back to a fixture in production.

---

## 4. `GET /api/taxa` -- the other fixture

[TaxonCombobox.svelte:3](../../src/lib/components/TaxonCombobox.svelte#L3) imports
`src/lib/fixtures/taxa.json` (249 KB, 1,835 rows) directly and filters it
client-side. It is a second hardcoded table, and it goes away in the same piece of work.

**The contract is spec 10, and it needs no changes.** `GET /api/taxa?rank=genus&q=sor&limit=10`
returns `{ "matches": [ { rank, name, record_count, parent_rank, parent_name } ] }`, sorted by
`record_count` descending, then name ascending. The service enforces `q` at 2 or more characters server-side, and it
caps `limit` at 20. It sets `Cache-Control: public, max-age=3600`, and it limits requests to 120 per minute per IP.

The backend must honor two notes from the frontend:

- The component already implements spec 10's client behavior: a prefix match on the whole name,
  2 or more characters, filtered by rank, capped at 10 results. The component also **drops an exact match**,
  because suggesting back what the user already typed adds noise. This filtering stays client-side of the
  response. The backend does not need to replicate it.
- `parent_rank` and `parent_name` are `null` for `scientific_name` rows, and non-null elsewhere.
  The UI shows only name and count today, but spec 10's homonym disambiguation
  (`Arvicolinae (subfamily, in Cricetidae)`) depends on these fields. Serve them.

The combobox debounce must move to 200 ms with in-flight requests cancelled (spec 10) once it
starts hitting the network. It has no debounce today, because a local array needs none.

---

## 5. Where `snapshot_date` comes from

D2 means the data is stale by design, so the UI states that fact. The source is an ETL-written metadata
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

The alias target's creation date was the alternative source, and this spec rejects it. That date records when the index
was *built*, not when the data was *extracted*. Those two times differ by however long the ETL queue
runs. `extracted_at` depends on the dump carrying a timestamp (request 4, spec 11). Until that
change lands, `extracted_at` equals the ETL run time. This is why the UI shows the date only.

Carrying the D29/D35 assertion results in this document lets `/api/schema` eventually
surface "this snapshot was verified" as a live claim, instead of leaving the guarantee in a build log nobody reads.

---

## 6. Frontend changes this unblocks

These changes are small. This list tells the backend what is waiting on it:

1. `src/routes/search/+page.ts` -- replace the `schemaFixture` import with a fetch of
   `${__API_BASE__}/api/schema`, alongside the search fetch the page already makes. This adds one request per
   page load. SvelteKit's `load` deduplicates requests across client-side navigation.
2. `TaxonCombobox.svelte` -- replace the `taxa.json` import with a debounced, cancellable fetch
   of `/api/taxa`.
3. `src/lib/types.ts` -- add `limits` (including `page_size`) to `Schema`. Delete `PAGE_SIZE` and
   `MAX_RESULT_WINDOW` from `+page.svelte`.
4. Delete `src/lib/fixtures/` and drop the fixture note from the homepage. `build_fixtures.py`
   stays. It serves as the reference implementation of this contract and as the e2e tests' data source.

`DETECTION_TYPES` and `SCIENTIFIC_NAME` in `types.ts` stay hardcoded on purpose. The first is a
query-shape mapping (which index field a detection type writes to). The second is a sentinel for
a non-rank. Neither value is vocabulary, and neither belongs in `/api/schema`.

---

## 7. Tests the backend owes

- Startup against an **empty index** yields a schema with empty aggregation lists, full
  code-table lists, and no panic.
- A refresh failure preserves the previous schema and logs the failure.
- `ranks` omits a rank with no values in the index. `scientific_name` still appears.
- No `public: 0` attribute type appears in `attribute_types`. All three such types appear in
  `nonpublic_types_dropped`.
- Every non-null `attribute_types[].vocabulary` value is a key of `vocabularies`.
- `vocabularies.ctexamined_detected` has all 32 values. `ectoparasite` sorts before
  `ectoparasite: flea`. A value with zero records is present, not omitted.
- `countries`, `states`, and `guid_prefixes` sort count-descending, and their aggregations report
  `sum_other_doc_count == 0`.
- The response deserializes into the fixture's shape. Assert against a checked-in golden file built
  by `tools/build_fixtures.py`, so a shape drift fails the backend build rather than the form.
