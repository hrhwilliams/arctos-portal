#!/usr/bin/env python3
"""Rewrite the Arctos dump as Parquet, one directory per guid_prefix.

  python docs/build_parquet.py 2026-03-09_filtered_flat.csv [arctos_parquet]

Partitioned because DuckDB pushes a single `=` into a Parquet scan but not a
disjunction: `guid IN (...)`, which is what an export is, otherwise decodes all
154 columns of the whole dump however few rows it wants. Split by collection,
a filter on guid_prefix skips whole files by their directory name.

Reading it back needs hive_partitioning, since the prefix lives in the path
rather than the files:

  SELECT * FROM read_parquet('arctos_parquet/**/*.parquet', hive_partitioning = true)
"""
import sys
import time
from pathlib import Path

import duckdb


def compact(conn, out):
    """One file per partition. The flush threshold that keeps the write inside
    memory leaves ~27 files per partition, and every query pays ~0.95s of glob
    and footer reads for it (note 04 §3). Read without hive_partitioning so the
    prefix stays in the directory name, not the files."""
    for part_dir in sorted(Path(out).iterdir()):
        files = sorted(part_dir.glob("*.parquet"))
        if len(files) <= 1:
            continue
        tmp = part_dir / "compact.tmp"
        conn.execute(
            f"""COPY (SELECT * FROM read_parquet('{part_dir.as_posix()}/*.parquet'))
                TO '{tmp.as_posix()}' (FORMAT parquet, COMPRESSION zstd, COMPRESSION_LEVEL 9)"""
        )
        for f in files:
            f.unlink()
        tmp.rename(part_dir / "data_0.parquet")


if __name__ == "__main__":
    if sys.argv[1] == "--compact":
        out = sys.argv[2] if len(sys.argv) > 2 else "arctos_parquet"
        conn = duckdb.connect()
        now = time.time()
        compact(conn, out)
        print(f"compacted {out} in {time.time() - now:.0f}s")
        sys.exit()
    else:
        csv_file = sys.argv[1]
        out = sys.argv[2] if len(sys.argv) > 2 else "arctos_parquet"

        conn = duckdb.connect()
        # A partitioned write buffers rows per partition per thread before flushing, and
        # the default 524,288 rows across ~300 collections is tens of gigabytes of wide
        # rows held at once. Flushing early costs several files per partition, which the
        # glob reads all the same.
        conn.execute("""
            SET preserve_insertion_order = false;
            SET temp_directory = '.tmp';
            SET memory_limit = '32GB';
            SET partitioned_write_flush_threshold = 100000;
            SET partitioned_write_max_open_files = 25;
        """)

        # Every column as text, deliberately. Elasticsearch does the filtering; DuckDB
        # only groups text columns at startup and copies whole rows back out as CSV, so
        # a detected type buys nothing and costs fidelity on the way out — leading
        # zeros, 1.20 -> 1.2, reformatted dates, empty string against null. It also
        # means no row can fail to parse, where `store_rejects` would have dropped it
        # from the dump without saying so.
        now = time.time()
        conn.execute(
            f"""COPY (SELECT * FROM read_csv(?, all_varchar = true))
                TO '{out}' (FORMAT parquet, PARTITION_BY guid_prefix,
                            COMPRESSION zstd, COMPRESSION_LEVEL 9)""",
            [csv_file],
        )
        print(f"wrote {out} in {time.time() - now:.0f}s")

        now = time.time()
        compact(conn, out)
        print(f"compacted {out} in {time.time() - now:.0f}s")

        rows, prefixes = conn.execute(
            f"SELECT count(*), count(DISTINCT guid_prefix) "
            f"FROM read_parquet('{out}/**/*.parquet', hive_partitioning = true)"
        ).fetchone()
        print(f"{rows:,} rows across {prefixes} collections")
