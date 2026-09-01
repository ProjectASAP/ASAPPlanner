#!/usr/bin/env python3
"""Summarize POST_ASAP_MISS lines emitted by the corpus test."""

from __future__ import annotations

import argparse
import re
from collections import Counter, defaultdict
from pathlib import Path


def category(query: str) -> str:
    query = query.strip()
    if query.startswith("label_values("):
        return "Grafana label_values variable"
    if query.startswith("query_result("):
        return "Grafana query_result variable"
    if query == "prometheus" or re.fullmatch(r"\d+[smhd]", query):
        return "dashboard variable value"
    if re.match(r"^[A-Za-z_][A-Za-z0-9_]*\(", query):
        return f"function: {query.split('(', 1)[0]}"
    if query.startswith("("):
        return "parenthesized expression / binary"
    if "{" in query and re.search(r"}\s*(?:[=!<>+*/-]|$)", query):
        return "selector or selector-root binary"
    return "metric selector / scalar"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("test_output", type=Path)
    args = parser.parse_args()
    misses: dict[str, Counter[str]] = defaultdict(Counter)
    for line in args.test_output.read_text().splitlines():
        if not line.startswith("POST_ASAP_MISS\t"):
            continue
        _, corpus, query = line.split("\t", 2)
        misses[corpus][category(query)] += 1

    for corpus, counts in misses.items():
        print(corpus)
        for pattern, count in counts.most_common():
            print(f"  {count:3} {pattern}")


if __name__ == "__main__":
    main()
