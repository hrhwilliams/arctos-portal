#!/usr/bin/env python3
"""Generate frontend fixtures from the real Arctos code tables and the sample slice.

Stands in for `GET /api/schema` and `GET /api/taxa` until the Rust service exists.
Everything here is real data, so the form exercises real vocabulary sizes and
real value shapes rather than invented ones.

  python tools/build_fixtures.py

Writes src/lib/fixtures/schema.json and src/lib/fixtures/taxa.json.
"""
import csv
import json
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CT = ROOT / "docs" / "data" / "code-tables"
OUT = ROOT / "src" / "lib" / "fixtures"

# Block-1 name selectors, mapped to their source column in the dump, from the
# same rank table the ingest and the service read (src/ranks.json).
# `scientific_name` is not a rank; it is the canonical way into this data (a
# Latin binomial) and shares the same (field, value) shape, so it rides in the
# same dropdown and is listed first.
_RANK_TABLE = json.loads((ROOT / "src" / "ranks.json").read_text(encoding="utf-8"))
RANKS = [("scientific_name", "Scientific name", "scientific_name")] + [
    (r["id"], r["label"], r["column"]) for r in _RANK_TABLE["ranks"]
]

# keys that are metadata on a code-table row, never the value itself
NON_VALUE_KEYS = {
    "description", "issue_url", "documentation_url", "collections",
    "recommend_for_collection_type", "search_terms", "value_code_table",
    "unit_code_table", "public", "definition", "collection_type",
}


def load_table(name):
    path = CT / f"{name}.json"
    if not path.exists():
        return None
    with path.open(encoding="utf-8") as fh:
        doc = json.load(fh)
    return doc["data"] if isinstance(doc, dict) and "data" in doc else doc


def value_key(rows):
    """The column holding the controlled value. Varies per table; take the first
    key that is not row metadata."""
    for key in rows[0]:
        if key not in NON_VALUE_KEYS:
            return key
    return None


def first_str(v):
    if isinstance(v, list):
        return v[0] if v else ""
    return v or ""


def build_vocabularies(types):
    """One entry per distinct value_code_table referenced by an attribute type."""
    vocab = {}
    for t in types:
        table = (t.get("value_code_table") or "").strip()
        if not table or table in vocab:
            continue
        rows = load_table(table)
        if not rows:
            continue
        key = value_key(rows)
        if not key:
            continue
        values = []
        for row in rows:
            val = (row.get(key) or "").strip()
            if not val:
                continue
            values.append({
                "value": val,
                "description": (row.get("description") or "").strip(),
                "documentation_url": first_str(row.get("documentation_url")),
            })
        # parents before children, so the form can indent the `category: term`
        # hierarchy the ancestor expansion depends on (D22)
        values.sort(key=lambda v: (v["value"].count(":"), v["value"].lower()))
        vocab[table] = values
    return vocab


# the parent chain is a taxonomic hierarchy; scientific_name is not part of it
RANK_CHAIN = [r for r in RANKS if r[0] != "scientific_name"]


def build_taxa(sample, profile):
    """Distinct (rank, name) with record counts, from the 199-row sample.

    Scientific names come from the full-slice profile instead, since the
    binomial is the field users reach for first and 199 rows is too thin.
    """
    if not sample.exists():
        return []
    counts = Counter()
    parents = {}
    order = [r[0] for r in RANK_CHAIN]
    with sample.open(encoding="utf-8-sig", newline="") as fh:
        for row in csv.DictReader(fh):
            for i, (rank_id, _, column) in enumerate(RANK_CHAIN):
                raw = (row.get(column) or "").strip()
                if not raw:
                    continue
                for name in (p.strip() for p in raw.split(";")):
                    if not name:
                        continue
                    counts[(rank_id, name)] += 1
                    if i > 0 and (rank_id, name) not in parents:
                        prank = order[i - 1]
                        pcol = RANK_CHAIN[i - 1][2]
                        pval = (row.get(pcol) or "").split(";")[0].strip()
                        if pval:
                            parents[(rank_id, name)] = (prank, pval)
    out = []
    for row in (profile.get("scientific_name_top") or []):
        out.append({
            "rank": "scientific_name",
            "name": row["name"],
            "record_count": row["count"],
            "parent_rank": None,
            "parent_name": None,
        })
    for (rank_id, name), n in counts.most_common():
        parent = parents.get((rank_id, name))
        out.append({
            "rank": rank_id,
            "name": name,
            "record_count": n,
            "parent_rank": parent[0] if parent else None,
            "parent_name": parent[1] if parent else None,
        })
    return out


def load_profile():
    path = ROOT / "docs" / "data" / "profiles" / "prefix_sciname.json"
    if not path.exists():
        return {}
    with path.open(encoding="utf-8") as fh:
        return json.load(fh)


def main():
    profile = load_profile()
    types = load_table("ctattribute_type")
    if not types:
        sys.exit("ctattribute_type.json not found")

    # D35 — types Arctos marks non-public never reach the client at all
    public = [t for t in types if int(t.get("public", 1)) == 1]
    dropped = [t["attribute_type"] for t in types if int(t.get("public", 1)) != 1]

    vocab = build_vocabularies(public)

    attribute_types = sorted(
        (
            {
                "id": t["attribute_type"],
                "label": t["attribute_type"],
                "description": (t.get("description") or "").strip(),
                "vocabulary": (t.get("value_code_table") or "").strip() or None,
                "units_table": (t.get("unit_code_table") or "").strip() or None,
            }
            for t in public
        ),
        key=lambda t: t["id"].lower(),
    )

    schema = {
        "snapshot_date": "2026-03-09",
        "fixture": True,
        "ranks": [{"id": i, "label": l, "field": f} for i, l, f in RANKS],
        "attribute_types": attribute_types,
        "vocabularies": vocab,
        # no code table exists for either; both are aggregations in the real service
        "countries": profile.get("country") or [],
        "states": profile.get("state_prov") or [],
        # `relations.relationship` in the mapping — how this record relates to
        # another cataloged item. A code table, so it is complete here.
        "relations": sorted(
            (
                {
                    "value": r["id_references"],
                    "description": (r.get("description") or "").strip(),
                }
                for r in (load_table("ctid_references") or [])
            ),
            key=lambda r: r["value"].lower(),
        ),
        "sorts": [
            {"id": "guid_asc", "label": "Catalog number"},
            {"id": "date_desc", "label": "Newest collection date"},
            {"id": "date_asc", "label": "Oldest collection date"},
        ],
        "guid_prefixes": [
            {"value": p["value"], "count": p["count"],
             "institution": p.get("institution", ""),
             "collection_cde": p.get("collection_cde", "")}
            for p in (profile.get("guid_prefix") or [])
        ],
        "limits": {"max_per_page": 200, "max_result_window": 10000, "max_export_rows": 100000},
        "nonpublic_types_dropped": dropped,
    }

    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / "schema.json").write_text(json.dumps(schema, indent=1), encoding="utf-8")
    taxa = build_taxa(ROOT / "docs" / "data" / "dataset" / "test.csv", profile)
    (OUT / "taxa.json").write_text(json.dumps(taxa, indent=1), encoding="utf-8")

    print(f"attribute types : {len(attribute_types)} public, {len(dropped)} dropped {dropped}")
    print(f"vocabularies    : {len(vocab)} tables, "
          f"{sum(len(v) for v in vocab.values())} values")
    print(f"collections     : {len(schema['guid_prefixes'])} guid_prefix values")
    print(f"places          : {len(schema['countries'])} countries, {len(schema['states'])} states")
    print(f"taxa            : {len(taxa)} distinct (rank, name)")


if __name__ == "__main__":
    main()
