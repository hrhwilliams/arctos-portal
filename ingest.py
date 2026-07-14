import csv, json, sys
from elasticsearch import Elasticsearch, helpers


csv.field_size_limit(sys.maxsize)

ES_HOST = "http://localhost:9200"
INDEX_NAME = "arctos"
CSV_FILE_PATH = "msb.csv"
MAPPINGS = {
    # only index the following fields in properties
    "dynamic": False,
    "properties": {
        "scientific_name": {
            "type": "text", 
            "fields": {"keyword": {"type": "keyword"}}
        },
        "guid": {"type": "keyword"},
        "guid_prefix": {"type": "keyword"},
        "coordinates": {"type": "geo_point"},
        "relatedcatalogeditems": {"type": "text"},
        "began_date": {"type": "date"},
        "partdetail": {
            "type": "nested",
            "properties": {
                "part_name": {"type": "keyword"},
                "part_attributes": {
                    "type": "nested",
                    "properties": {
                        "attribute_type": {"type": "keyword"},
                        "attribute_value": {"type": "text", "fields": {"keyword": {"type": "keyword"}}}
                    }
                }
            }
        },
        "attributedetail": {
            "type": "nested",
            "properties": {
                "attribute_type": {"type": "keyword"},
                "attribute_value": {"type": "text", "fields": {"keyword": {"type": "keyword"}}}
            }
        }
    }
}
SETTINGS = {
    # local dev container doesn't need replicas or sharding
    "number_of_shards": 1,
    "number_of_replicas": 0
}


def parse_row(row):
    parsed_row = {}
    for key, value in row.items():
        if not value:
            parsed_row[key] = None
            continue

        value = value.strip()

        if (value.startswith('{') and value.endswith('}')) or \
           (value.startswith('[') and value.endswith(']')):
            try:
                parsed_row[key] = json.loads(value)
            except json.JSONDecodeError:
                parsed_row[key] = value
        else:
            parsed_row[key] = value

    lat = parsed_row.get("dec_lat")
    lon = parsed_row.get("dec_long")
    if lat and lon:
        parsed_row["coordinates"] = {"lat": float(lat), "lon": float(lon)}

    return parsed_row


def generate_actions(csv_file, index_name):
    with open(csv_file, mode='r', encoding='utf-8') as f:
        reader = csv.DictReader(f)
        for row in reader:
            yield {
                "_index": index_name,
                "_id": row["collection_object_id"],
                "_source": parse_row(row)
            }


def main():
    es = Elasticsearch(hosts=[ES_HOST], request_timeout=60)

    if not es.indices.exists(index=INDEX_NAME):
        es.indices.create(
            index=INDEX_NAME,
            mappings=MAPPINGS,
            settings=SETTINGS
        )

    try:
        helpers.bulk(
            es,
            generate_actions(CSV_FILE_PATH, INDEX_NAME),
            chunk_size=1000,
            raise_on_error=True
        )
    except helpers.BulkIndexError as e:
        print(json.dumps(e.errors[0], indent=2))


if __name__ == "__main__":
    main()
