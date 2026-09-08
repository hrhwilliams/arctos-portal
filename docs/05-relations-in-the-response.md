# Relations in the search response

The related records for a search are already inside the records the search returns.
Nothing needs to be fetched, joined, or counted separately — and anything that does
fetch separately drifts out of step with the filters.

## The rule

Elasticsearch stores relations _inside_ the specimen document, not in a separate
collection. Every record in `results.records` carries its own `relations` array. The
parasites for the records you were given are, by construction, the relations inside
those records.

Two consequences:

- Scope filters need no special handling. Narrowing to one collection removes records
  from the response, and their relations leave with them.
- The service filters **records**, not relations. A specimen that matched on one
  Cestoda pairing still arrives carrying its nematodes, its mites, and its littermates.
  The client filters the array itself.

## What a relation looks like

One real relation, from the response to the query below. Keys whose value is empty are
omitted entirely, so read every field defensively.

```json
{
  "relationship":            "host of parasite",
  "related_guid":            "DMNS:Para:581",
  "related_identifier":      "https://arctos.database.museum/guid/DMNS:Para:581",
  "related_identifier_type": "Arctos record GUID",
  "related_identification":  "Cestoda",
  "related_geography":       "United States, Colorado, Eagle County",
  "related_phylum":          "Platyhelminthes",
  "related_phylclass":       "Cestoda"
}
```

The `related_<rank>` fields are the related record's rank chain, resolved at index time
from its identification name. A record identified only to a class has no genus, so the
ranks below the one it was identified to are absent. That is data, not a gap to fill in.

## The predicate

To show only the relations a search asked for, apply the same test the service applied
to select the records. A taxon row is `rank|name|relationships|rank|name|…`; its
relationship terms and its related taxa map onto these fields:

| From the taxon row            | Relation field            | Match             |
| ----------------------------- | ------------------------- | ----------------- |
| relationship (segment 3)      | `relationship`            | exact, any of them |
| related `scientific_name`     | `related_identification`  | whole name        |
| related `phylum`              | `related_phylum`          | exact             |
| related `class`               | `related_phylclass`       | exact             |
| related `order`               | `related_phylorder`       | exact             |
| related `family`              | `related_family`          | exact             |
| related `subfamily`           | `related_subfamily`       | exact             |
| related `genus`               | `related_genus`           | exact             |
| related `species`             | `related_species`         | exact             |

`class` and `order` land on `related_phylclass` and `related_phylorder` — the index
keeps the Arctos column names. Comparisons are case-insensitive on the service side, so
lower-case both sides rather than using `===` on the raw strings.

Combining rules:

- Relationship terms within a row **OR**. So do related taxa within a row.
- The relationship and the related taxon must hold for the **same** relation, not for
  two different ones.
- Taxon rows OR with each other: a relation qualifies if it satisfies any one row.
- A row with no relationship and no related taxon puts no condition on relations at all.
  It contributes nothing to the panel.

## Deriving the list

```js
const RELATED_FIELD = {
  scientific_name: "related_identification",
  phylum:          "related_phylum",
  class:           "related_phylclass",
  order:           "related_phylorder",
  family:          "related_family",
  subfamily:       "related_subfamily",
  genus:           "related_genus",
  species:         "related_species",
};

const same = (a, b) => (a ?? "").trim().toLowerCase() === b.trim().toLowerCase();

// row: { relationships: string[], related: { rank, name }[] } — the decoded taxon row
function matchesRow(relation, row) {
  if (row.relationships.length &&
      !row.relationships.some((r) => same(relation.relationship, r))) {
    return false;
  }
  if (!row.related.length) return true;
  return row.related.some(({ rank, name }) =>
    same(relation[RELATED_FIELD[rank]], name));
}

// rows: every taxon row of the current search
export function matchedRelations(results, rows) {
  const constrained = rows.filter((r) => r.relationships.length || r.related.length);
  if (!constrained.length) return [];

  return results.records.flatMap((record) =>
    (record.relations ?? [])
      .filter((rel) => constrained.some((row) => matchesRow(rel, row)))
      // keep the host, or the panel cannot say whose parasite this is
      .map((rel) => ({ ...rel, host_guid: record.guid })));
}
```

The only input besides the response is the decoded taxon rows, which the query layer
already has. This needs no second request and no state from the previous search.

## Numbers to check against

Both queries run against the live service. Use them as a fixture: if the panel disagrees
with the last column, it is not reading the response it was given.

| Query                                                | total | records | relations | matching |
| ---------------------------------------------------- | ----: | ------: | --------: | -------: |
| `taxon=genus\|Sorex\|host of parasite\|class\|Cestoda` |   227 |     100 |       176 |      102 |
| the same, plus `prefix=DMNS:Mamm`                     |    17 |      17 |        26 |       17 |

The gap between 26 and 17 in the second row is the point of the predicate: nine of those
relations belong to matching specimens but are not what the search asked for.

**Known symptom.** A panel reporting 182 parasites for the filtered search is showing
the unfiltered result. The filtered response contains 26 relations in total; 182 cannot
be derived from it under any rule. Suspect a cached panel, or a second request that
carries only the `taxon` parameter.

## A page is not the result set

`results.records` holds one page — 100 records — while `results.total.value` counts the
whole match. In the unfiltered query above, the 102 matching relations come from the 100
records on page 1, not from all 227. Page 2 has different ones.

So the panel can honestly say "parasites on this page" for free. A count over the entire
result set is a different thing and cannot be computed from any single response: that
needs a nested aggregation on the service side. Ask for it if the panel needs to make
the stronger claim.

`total.relation` is `eq` or `gte`. Past 10,000 matches the count is capped, and the UI
should render "10,000+" rather than a number that is quietly wrong.

---

Fields as of the current index build. The rank chain (`related_<rank>`) requires the
reindex that added it — 5 relations out of 288,642 have no chain, and those match only
by `related_identification`.
