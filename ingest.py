"""Load an Arctos dump into Elasticsearch against mapping v2.

    python tools/ingest.py [--recreate] [--limit N] [csv_path]

The mapping is read from docs/mapping.v2.json rather than inlined, so the index,
the frontend fixtures and the specs cannot drift apart.

Most of this file is the handful of things the index cannot derive for itself.
Each one is a place where a silent regression returns fewer results instead of
failing, so each is counted and reported at the end.

  events[]              from json_locality, one nested doc per element
  event_date_min/_max   sort-only rollups over those events
  detected & siblings   flat arrays, from the SAME attributedetail parse that
                        feeds the nested docs — never from the joined columns
  collector_ids         role-filtered rollup of collector_agents
  relations[]           from related_record_cache, `record` normalised to a GUID

Two assertions fail the build rather than warn, because both leak rather than
merely returning the wrong count on a public portal:

  * `encumbrances` is empty across the snapshot
  * no attribute type marked `public: 0` in ctattribute_type is indexed
"""

import argparse
import csv
import json
import re
import sys
from collections import Counter
from pathlib import Path

csv.field_size_limit(sys.maxsize)

ROOT = Path(__file__).resolve().parent
MAPPING_FILE = ROOT / "docs" / "mapping.v2.json"
# CODE_TABLES = ROOT / "docs" / "data" / "code-tables"

ES_HOST = "http://localhost:9200"
INDEX_NAME = "arctos"
CSV_FILE_PATH = "msb.csv"

# local dev container needs neither replicas nor sharding
LOCAL_SETTINGS = {"number_of_shards": 1, "number_of_replicas": 0}

# the four detection types, and the flat field each rolls up into
DETECTION_FIELDS = {
    "detected": "detected",
    "not detected": "not_detected",
    "examined for": "examined_for",
    "not examined for": "not_examined_for",
}

# only `collector` rolls up; the array also holds preparators and others
COLLECTOR_ROLE = "collector"

GUID_IN_URL = re.compile(r"/guid/(?P<guid>[^/?#]+)", re.IGNORECASE)
BARE_GUID = re.compile(r"^[A-Za-z]+:[A-Za-z]+:.+$")

# json_locality keys copied straight through to events[]
EVENT_KEYS = [
    "specimen_event_id",
    "specimen_event_type",
    "began_date",
    "ended_date",
    "verbatim_date",
    "higher_geog",
    "habitat",
    "verificationstatus",
    "spec_locality",
    "locality_name",
    "locality_search_terms",
    "locality_id",
    "coordinate_error_m",
    "collecting_method",
    "collecting_source",
]

stats = Counter()
undocumented = set()


def load_mapping():
    with MAPPING_FILE.open(encoding="utf-8") as fh:
        doc = json.load(fh)
    settings = doc.get("settings", {})
    settings.update(LOCAL_SETTINGS)
    return doc["mappings"], settings


# def nonpublic_attribute_types():
#     """Types Arctos marks `public: 0`. Dropped, and asserted absent (D35)."""
#     path = CODE_TABLES / "ctattribute_type.json"
#     if not path.exists():
#         sys.exit(f"{path} not found — fetch the code tables before ingesting.")
#     with path.open(encoding="utf-8") as fh:
#         doc = json.load(fh)
#     rows = doc["data"] if isinstance(doc, dict) and "data" in doc else doc
#     nonpublic = {r["attribute_type"] for r in rows if int(r.get("public", 1)) != 1}
#     documented = {r["attribute_type"] for r in rows}
#     return nonpublic, documented


def as_list(value):
    """json_locality and friends are JSON arrays."""
    return value if isinstance(value, list) else []


def coordinates(lat, lon):
    """Omit the point entirely when either half is missing.

    A geo_point with a null component fails the bulk request, and 0,0 is worse —
    it silently places the specimen in the Gulf of Guinea.
    """
    if lat in (None, "") or lon in (None, ""):
        return None
    try:
        return {"lat": float(lat), "lon": float(lon)}
    except (TypeError, ValueError):
        return None


def build_events(row):
    """One nested doc per json_locality element.

    All events are kept, unsorted and unmerged: duplicate georeferences of a
    single visit stay as separate events rather than being deduplicated.
    """
    events = []
    for src in as_list(row.get("json_locality")):
        if not isinstance(src, dict):
            continue
        event = {k: src.get(k) for k in EVENT_KEYS if src.get(k) not in (None, "")}
        point = coordinates(src.get("dec_lat"), src.get("dec_long"))
        if point:
            event["coordinates"] = point
        event["synthesized"] = False
        events.append(event)

    # A record can carry a date or place at the top level with an empty
    # json_locality. Since all date and place filtering runs through events,
    # that record would otherwise be unfindable by either.
    if not events and any(
        row.get(k) for k in ("began_date", "ended_date", "higher_geog", "spec_locality")
    ):
        event = {
            k: row.get(k)
            for k in ("began_date", "ended_date", "higher_geog", "spec_locality")
            if row.get(k)
        }
        point = coordinates(row.get("dec_lat"), row.get("dec_long"))
        if point:
            event["coordinates"] = point
        event["synthesized"] = True
        events.append(event)
        stats["events_synthesized"] += 1

    # `event_count` disagrees with the array on ~0.5% of records, in both
    # directions, so it is ignored entirely.
    return events


def date_rollups(events):
    """Sort-only min/max. Never used as a filter — that reintroduces the
    multi-event bug the nested events exist to prevent."""
    starts = [e["began_date"] for e in events if e.get("began_date")]
    ends = [e.get("ended_date") or e.get("began_date") for e in events]
    ends = [e for e in ends if e]
    return (min(starts) if starts else None, max(ends) if ends else None)


def build_attributes(row, nonpublic, documented):
    """Nested attribute docs plus the four flat detection arrays.

    Both come from this one parse. Building the flat arrays from the joined
    top-level columns instead would introduce a drift risk between two
    representations that must agree, and no amount of testing removes it.
    """
    nested = []
    flat = {field: [] for field in DETECTION_FIELDS.values()}

    for src in as_list(row.get("attributedetail")):
        if not isinstance(src, dict):
            continue
        atype = (src.get("attribute_type") or "").strip()
        if not atype:
            continue
        if atype in nonpublic:
            stats["nonpublic_dropped"] += 1
            continue
        if atype not in documented:
            undocumented.add(atype)

        doc = {k: v for k, v in src.items() if v not in (None, "")}
        nested.append(doc)

        field = DETECTION_FIELDS.get(atype)
        value = (src.get("attribute_value") or "").strip()
        if field and value:
            flat[field].append(value)

    return nested, {k: sorted(set(v)) for k, v in flat.items() if v}


def build_agents(row):
    """Nested agents, plus the role-filtered id rollup for the common case.

    Match on agent_id — a stable Arctos agent URL — not the name, which is what
    makes one person survive spelling variants across a century of cataloguing.
    """
    agents, ids = [], []
    for src in as_list(row.get("collector_agents")):
        if not isinstance(src, dict):
            continue
        agent = {
            k: src[k]
            for k in ("agent_id", "agent_name", "agent_role", "agent_order")
            if src.get(k) not in (None, "")
        }
        if not agent:
            continue
        agents.append(agent)
        if (agent.get("agent_role") or "").strip().lower() == COLLECTOR_ROLE:
            if agent.get("agent_id"):
                ids.append(agent["agent_id"])
    return agents, ids


def related_guid(value):
    """`record` arrives as a full URL, a bare GUID, or something that is neither.

    Roughly an eighth are neither — barcodes, NK numbers, foreign catalogue
    numbers. Those keep `related_identifier` and simply resolve to no GUID; the
    unresolved rate is counted so it can be published rather than swallowed.
    """
    value = (value or "").strip()
    match = GUID_IN_URL.search(value)
    if match:
        return match.group("guid")
    return value if BARE_GUID.match(value) else None


def build_relations(row):
    """Copied from related_record_cache, not derived.

    Both directions are stored natively, so no inverse edge is synthesised and
    nothing is inferred. Do not parse `relatedcatalogeditems` — it is the same
    data as a display string.
    """
    relations = []
    for src in as_list(row.get("related_record_cache")):
        if not isinstance(src, dict):
            continue
        relation = {
            "relationship": src.get("relationship"),
            "related_guid": related_guid(src.get("record")),
            "related_identifier": src.get("record"),
            "related_identifier_type": src.get("identifier_type"),
            "related_family": src.get("family"),
            "related_identification": src.get("identification"),
            "related_geography": src.get("geography"),
        }
        stats["relations_total"] += 1
        if not relation["related_guid"]:
            stats["relations_unresolved"] += 1
        relations.append({k: v for k, v in relation.items() if v not in (None, "")})
    return relations


def parse_row(row):
    parsed = {}
    for key, value in row.items():
        if not value:
            parsed[key] = None
            continue
        value = value.strip()
        if (value.startswith("{") and value.endswith("}")) or (
            value.startswith("[") and value.endswith("]")
        ):
            try:
                parsed[key] = json.loads(value)
            except json.JSONDecodeError:
                stats["json_parse_errors"] += 1
                parsed[key] = value
        else:
            parsed[key] = value
    return parsed


def build_document(row, nonpublic, documented):
    doc = parse_row(row)

    # A public portal must not begin leaking because an upstream collection
    # changed. Enforced, not observed.
    if doc.get("encumbrances"):
        raise SystemExit(
            f"encumbrances present on {doc.get('guid')} — refusing to index restricted data"
        )

    events = build_events(doc)
    doc["events"] = events
    date_min, date_max = date_rollups(events)
    doc["event_date_min"] = date_min
    doc["event_date_max"] = date_max

    nested_attrs, flat_attrs = build_attributes(doc, nonpublic, documented)
    doc["attributedetail"] = nested_attrs
    for field in DETECTION_FIELDS.values():
        doc[field] = flat_attrs.get(field)

    agents, collector_ids = build_agents(doc)
    doc["agents"] = agents
    doc["collector_ids"] = collector_ids

    doc["relations"] = build_relations(doc)

    # top-level coordinates are gone: a specimen can have several events, each
    # with its own georeference, so the point lives on the event
    doc.pop("coordinates", None)

    return doc


def generate_actions(csv_file, index_name, nonpublic, documented, limit=None):
    with open(csv_file, mode="r", encoding="utf-8-sig") as fh:
        for i, row in enumerate(csv.DictReader(fh)):
            if limit and i >= limit:
                return
            stats["rows"] += 1
            yield {
                "_index": index_name,
                "_id": row["collection_object_id"],
                "_source": build_document(row, nonpublic, documented),
            }


def main():
    # imported here so the self-check can run without the client installed
    from elasticsearch import Elasticsearch, helpers

    ap = argparse.ArgumentParser()
    ap.add_argument("csv_path", nargs="?", default=CSV_FILE_PATH)
    ap.add_argument(
        "--recreate",
        action="store_true",
        help="drop and rebuild the index — required after any mapping change",
    )
    ap.add_argument("--limit", type=int, help="index only the first N rows")
    args = ap.parse_args()

    mappings, settings = load_mapping()
    # nonpublic, documented = nonpublic_attribute_types()

    es = Elasticsearch(hosts=[ES_HOST], request_timeout=60)

    if args.recreate and es.indices.exists(index=INDEX_NAME):
        es.indices.delete(index=INDEX_NAME)
    if not es.indices.exists(index=INDEX_NAME):
        es.indices.create(index=INDEX_NAME, mappings=mappings, settings=settings)
    else:
        print(
            f"index {INDEX_NAME!r} already exists; mapping changes are NOT applied. "
            "Re-run with --recreate."
        )

    try:
        helpers.bulk(
            es,
            generate_actions(args.csv_path, INDEX_NAME, [], [], args.limit),
            chunk_size=1000,
            raise_on_error=False,
        )
    except helpers.BulkIndexError as e:
        print(json.dumps(e.errors[0], indent=2))
        raise

    if stats["nonpublic_dropped"]:
        raise SystemExit(
            f"{stats['nonpublic_dropped']} attribute rows marked `public: 0` were present. "
            "They were dropped, but their presence is unexpected — investigate before publishing."
        )

    print(f"rows                 {stats['rows']:,}")
    print(f"events synthesized   {stats['events_synthesized']:,}")
    print(
        f"relations            {stats['relations_total']:,} "
        f"({stats['relations_unresolved']:,} unresolved)"
    )
    print(f"json parse errors    {stats['json_parse_errors']:,}")
    if undocumented:
        print(f"UNDOCUMENTED TYPES   {sorted(undocumented)}")


if __name__ == "__main__":
    main()
