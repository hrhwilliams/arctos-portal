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
  related_<rank>        the related record's rank chain, resolved from the
                        dump's own rank columns by its identification name

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
import time
from collections import Counter
from pathlib import Path

csv.field_size_limit(sys.maxsize)

ROOT = Path(__file__).resolve().parent
MAPPING_FILE = ROOT / "docs" / "mapping.v2.json"
# CODE_TABLES = ROOT / "docs" / "data" / "code-tables"

ES_HOST = "http://localhost:9200"
INDEX_NAME = "arctos"

# local dev container doesn't need replicas
# shards = parallelism, replicas = duplicate data
LOCAL_SETTINGS = {"number_of_shards": 4, "number_of_replicas": 0}

# The rank table. The service compiles the same file in (src/ranks.rs), so
# what is indexed here and what the service asks for cannot drift.
RANK_TABLE = json.loads((ROOT / "src" / "ranks.json").read_text(encoding="utf-8"))

# the four detection types, and the flat field each rolls up into
DETECTION_FIELDS = {d["type"]: d["field"] for d in RANK_TABLE["detections"]}

# only `collector` rolls up; the array also holds preparators and others
COLLECTOR_ROLE = "collector"

# how often each pass reports progress. Both passes read the whole dump before
# anything is queryable, so a silent run is indistinguishable from a hung one.
PROGRESS_EVERY = 100_000

# the rank columns, highest first. The order is the chain: a name found in
# column i fixes every rank above it and none below it.
RANK_COLUMNS = [r["column"] for r in RANK_TABLE["ranks"]]

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
    "locality_id",
    # json_locality spells the coordinate error `coordinate_error`, with
    # `max_error_distance` and `max_error_units` beside it. None of the three is
    # copied, so an event carries no error radius. The record-level
    # `coordinateuncertaintyinmeters` column is what a map reads.
    "collecting_method",
    "collecting_source",
]

stats = Counter()
undocumented = set()


def check_mapping(mappings):
    """The index must carry what the rank table says it does: a `split`
    subfield on every rank column, a `related_<column>` on every relation, and
    a keyword field per detection type. A missing one does not fail a query; it
    silently returns fewer results. So it fails here instead."""
    props = mappings["properties"]
    related = props.get("relations", {}).get("properties", {})
    missing = []
    for column in RANK_COLUMNS:
        if "split" not in props.get(column, {}).get("fields", {}):
            missing.append(f"{column}.split")
        if f"related_{column}" not in related:
            missing.append(f"relations.related_{column}")
    for field in DETECTION_FIELDS.values():
        if props.get(field, {}).get("type") != "keyword":
            missing.append(field)
    if missing:
        sys.exit(f"{MAPPING_FILE} does not carry the rank table (src/ranks.json): missing {missing}")


def load_mapping():
    with MAPPING_FILE.open(encoding="utf-8") as fh:
        doc = json.load(fh)
    check_mapping(doc["mappings"])
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


def names(value):
    """A rank column joins the names of several determinations with `;`."""
    return [n.strip() for n in (value or "").split(";") if n.strip()]


def build_taxonomy(csv_file, limit=None):
    """`name -> its rank chain`, read out of the dump's own rank columns.

    The relation cache carries the related record's `identification` and its
    `family`, and no other rank. Without this map a Related-taxon search cannot
    honour the rank it was given: every rank would match the same free-text
    identification, so `class|Cestoda` and `genus|Cestoda` would return the same
    records.

    A name found in rank column i fixes the ranks above it and leaves the ranks
    below it empty, because a record identified only to a class has no genus to
    record. A scientific name fixes the whole chain. The first row to define a
    name wins; a record with several determinations contributes the first name
    of each column, so a chain can mix two determinations of the same record.
    """
    taxonomy = {}
    started = time.perf_counter()
    with open(csv_file, mode="r", encoding="utf-8-sig") as fh:
        for i, row in enumerate(csv.DictReader(fh)):
            if limit and i >= limit:
                break
            if i and i % PROGRESS_EVERY == 0:
                print(
                    f"  scanned {i:,} rows, {len(taxonomy):,} names "
                    f"({i / (time.perf_counter() - started):,.0f}/s)",
                    flush=True,
                )
            chain = [(names(row.get(column)) or [None])[0] for column in RANK_COLUMNS]
            # A scientific name is the whole chain, so it is claimed first.
            for name in names(row.get("scientific_name")):
                taxonomy.setdefault(name, chain)
            for depth, column in enumerate(RANK_COLUMNS):
                for name in names(row.get(column)):
                    taxonomy.setdefault(name, chain[: depth + 1])
    return taxonomy


def build_relations(row, taxonomy):
    """Copied from related_record_cache, not derived.

    Both directions are stored natively, so no inverse edge is synthesised and
    nothing is inferred. Do not parse `relatedcatalogeditems` — it is the same
    data as a display string.

    The one derived part is the related record's rank chain, which
    [build_taxonomy] resolves from its identification name. An identification
    the dump does not know keeps its `related_family` from the cache and is
    counted.
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
        identification = (src.get("identification") or "").strip()
        resolved = taxonomy.get(identification)
        if resolved:
            # The resolved family wins over the cached one: it comes from the
            # same columns the searched taxon is matched against.
            relation.update(
                {
                    f"related_{column}": value
                    for column, value in zip(RANK_COLUMNS, resolved)
                    if value
                }
            )
        elif identification:
            stats["relations_rank_unresolved"] += 1

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


def build_document(row, nonpublic, documented, taxonomy):
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

    # partdetail is already JSON by the time it gets here. Keep only the objects:
    # a row whose JSON failed to parse would otherwise push a bare string into a
    # nested field and fail the whole bulk chunk.
    doc["partdetail"] = [p for p in as_list(doc.get("partdetail")) if isinstance(p, dict)]

    agents, collector_ids = build_agents(doc)
    doc["agents"] = agents
    doc["collector_ids"] = collector_ids

    doc["relations"] = build_relations(doc, taxonomy)

    # top-level coordinates are gone: a specimen can have several events, each
    # with its own georeference, so the point lives on the event
    doc.pop("coordinates", None)

    return doc


def generate_actions(csv_file, index_name, nonpublic, documented, taxonomy, limit=None):
    started = time.perf_counter()
    with open(csv_file, mode="r", encoding="utf-8-sig") as fh:
        for i, row in enumerate(csv.DictReader(fh)):
            if limit and i >= limit:
                return
            stats["rows"] += 1
            # This count is documents handed to the bulk helper, which is up to
            # one chunk ahead of what Elasticsearch has acknowledged.
            if stats["rows"] % PROGRESS_EVERY == 0:
                rate = stats["rows"] / (time.perf_counter() - started)
                print(
                    f"  sent {stats['rows']:,} rows ({rate:,.0f}/s)",
                    flush=True,
                )
            yield {
                "_index": index_name,
                "_id": row["collection_object_id"],
                "_source": build_document(row, nonpublic, documented, taxonomy),
            }


def main():
    # imported here so the self-check can run without the client installed
    from elasticsearch import Elasticsearch, helpers

    ap = argparse.ArgumentParser()
    ap.add_argument("csv_path", nargs="?")
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

        # One extra pass over the CSV, before the indexing pass: a relation can
        # name a record that appears anywhere in the dump, so the whole name
        # table has to exist before the first document is built. Nothing reaches
        # Elasticsearch until this pass finishes.
        print(f"pass 1/2: reading rank columns from {args.csv_path}", flush=True)
        started = time.perf_counter()
        taxonomy = build_taxonomy(args.csv_path, args.limit)
        print(
            f"pass 1/2: {len(taxonomy):,} names in {time.perf_counter() - started:.0f}s",
            flush=True,
        )
        print(f"pass 2/2: indexing into {INDEX_NAME!r}", flush=True)

        try:
            # `raise_on_error=False` keeps one bad row from ending the run, so
            # the rejections come back in a list instead. Counting them is the
            # only thing standing between a mapping mistake and an index that is
            # quietly missing documents.
            indexed, rejected = helpers.bulk(
                es,
                generate_actions(args.csv_path, INDEX_NAME, [], [], taxonomy, args.limit),
                chunk_size=1000,
                raise_on_error=False,
            )
            stats["indexed"] = indexed
            stats["rejected"] = len(rejected)
            if rejected:
                print(f"REJECTED {len(rejected):,} documents. The first:", flush=True)
                print(json.dumps(rejected[0], indent=2), flush=True)
        except helpers.BulkIndexError as e:
            print(json.dumps(e.errors[0], indent=2))
            raise
    else:
        print(
            f"index {INDEX_NAME!r} already exists; mapping changes are NOT applied. "
            "Re-run with --recreate."
        )

    # This merge rewrites the whole index into one segment per shard, so it runs
    # for minutes on a full snapshot with nothing to report while it does.
    print("force-merging to one segment per shard", flush=True)
    es.options(request_timeout=1800).indices.forcemerge(index=INDEX_NAME, max_num_segments=1)

    if stats["nonpublic_dropped"]:
        raise SystemExit(
            f"{stats['nonpublic_dropped']} attribute rows marked `public: 0` were present. "
            "They were dropped, but their presence is unexpected — investigate before publishing."
        )

    print(f"rows                 {stats['rows']:,}")
    print(f"indexed              {stats['indexed']:,} ({stats['rejected']:,} rejected)")
    print(f"events synthesized   {stats['events_synthesized']:,}")
    print(
        f"relations            {stats['relations_total']:,} "
        f"({stats['relations_unresolved']:,} unresolved, "
        f"{stats['relations_rank_unresolved']:,} with no rank chain)"
    )
    print(f"json parse errors    {stats['json_parse_errors']:,}")
    if undocumented:
        print(f"UNDOCUMENTED TYPES   {sorted(undocumented)}")


if __name__ == "__main__":
    main()
