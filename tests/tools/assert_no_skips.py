#!/usr/bin/env python3
"""Fail a CI run in which any suite skipped.

A skip means a dependency the runner was supposed to provide went missing, and
it reads as green. This turns that back into a red run, and additionally lists
the per-check skips suites record internally, which never reach the JUnit
report at all.

Usage: assert_no_skips.py <junit-xml> [...]
"""
import sys
import xml.parsers.expat
from typing import List, Optional


class Report:
    """The testcases of a JUnit report, read element by element: each one's name,
    its skip messages, and its captured output."""

    def __init__(self) -> None:
        self.cases: List[dict] = []
        self._case: Optional[dict] = None
        self._out: Optional[List[str]] = None

    def start(self, tag: str, attrs: dict) -> None:
        if tag == "testcase":
            self._case = {"name": attrs.get("name", "?"), "skipped": [], "out": []}
            self.cases.append(self._case)
        elif self._case is not None and tag == "skipped":
            self._case["skipped"].append(attrs.get("message") or "")
        elif self._case is not None and tag == "system-out":
            self._out = []

    def end(self, tag: str) -> None:
        if tag == "testcase":
            self._case = None
        elif tag == "system-out" and self._case is not None and self._out is not None:
            self._case["out"].append("".join(self._out))
            self._out = None

    def text(self, data: str) -> None:
        if self._out is not None:
            self._out.append(data)


def read(path: str) -> List[dict]:
    """The testcases of the report at `path`. A JUnit report declares no document
    type, so one that does is refused, and no parameter entity is ever read."""
    report = Report()
    parser = xml.parsers.expat.ParserCreate()
    parser.SetParamEntityParsing(xml.parsers.expat.XML_PARAM_ENTITY_PARSING_NEVER)

    def doctype(*_: object) -> None:
        raise ValueError(f"{path}: unexpected document type declaration")

    parser.StartDoctypeDeclHandler = doctype
    parser.StartElementHandler = report.start
    parser.EndElementHandler = report.end
    parser.CharacterDataHandler = report.text
    with open(path, "rb") as fh:
        parser.ParseFile(fh)
    return report.cases


skipped, per_check = [], []
for path in sys.argv[1:]:
    for case in read(path):
        name = case["name"]
        for message in case["skipped"]:
            skipped.append(f"{name}: {message.strip()[:160]}")
        for out in case["out"]:
            for line in out.splitlines():
                if line.startswith("SKIP  ["):
                    per_check.append(f"{name}: {line[6:].strip()[:160]}")

for line in per_check:
    print(f"note: check not observed here -- {line}")
if not skipped:
    print(f"no skipped suites ({len(per_check)} unobserved checks)")
    sys.exit(0)

print(f"\n{len(skipped)} suite(s) skipped, which this run does not allow:")
for line in skipped:
    print(f"  {line}")
sys.exit(1)
