#!/usr/bin/env python3
"""Compare cached formula values in Excel and Sheets exports without evaluating them."""

import argparse
from collections import Counter
import math
import posixpath
import sys
import xml.etree.ElementTree as ET
import zipfile


def sheet_parts(package):
    workbook = ET.fromstring(package.read("xl/workbook.xml"))
    rels = ET.fromstring(package.read("xl/_rels/workbook.xml.rels"))
    targets = {
        rel.attrib["Id"]: (
            target.lstrip("/") if target.startswith("/") else posixpath.normpath(f"xl/{target}")
        )
        for rel in rels
        if rel.attrib["Type"].endswith("/worksheet")
        for target in [rel.attrib["Target"]]
    }
    for sheet in workbook.findall("./{*}sheets/{*}sheet"):
        for key, rel in sheet.attrib.items():
            if key.endswith("}id") and rel in targets:
                yield sheet.attrib["name"], targets[rel]


def cached_cells(path, reference_addresses=()):
    with zipfile.ZipFile(path) as package:
        strings = []
        if "xl/sharedStrings.xml" in package.namelist():
            table = ET.fromstring(package.read("xl/sharedStrings.xml"))
            strings = [string_item(cell) for cell in table]
        for sheet, part in sheet_parts(package):
            with package.open(part) as xml:
                for _, row in ET.iterparse(xml, events=("end",)):
                    if not row.tag.endswith("}row"):
                        continue
                    for cell in row:
                        is_formula = cell.find("{*}f") is not None
                        address = (sheet, cell.attrib["r"])
                        if not is_formula and address not in reference_addresses:
                            continue
                        value = cell.find("{*}v")
                        raw = value.text if value is not None else None
                        kind = cell.get("t", "n")
                        if kind == "inlineStr":
                            result = ("text", string_item(cell.find("{*}is")))
                        elif kind == "str" and value is not None:
                            result = ("text", raw or "")
                        elif raw is None:
                            result = ("uncached", None)
                        elif kind == "s":
                            result = ("text", strings[int(raw)])
                        elif kind == "n":
                            result = (kind, float(raw))
                        else:
                            result = (kind, raw)
                        yield address, (is_formula, result)
                    row.clear()


def string_item(cell):
    return "".join(
        node.text or "" for node in cell.findall("{*}t") + cell.findall("{*}r/{*}t")
    )


def equivalent(expected, actual):
    if expected[0] != actual[0]:
        return False
    if expected[0] == "n":
        return math.isclose(expected[1], actual[1], rel_tol=1e-9, abs_tol=1e-10)
    return expected == actual


def compare(reference, result):
    expected = {address: value for address, (_, value) in cached_cells(reference)}
    actual = dict(cached_cells(result, expected))
    counts = Counter()
    examples = []
    for address, value in expected.items():
        is_formula, saved_value = actual.get(address, (False, None))
        if saved_value is None:
            reason = "missing"
        elif value[0] == "uncached":
            reason = "uncached_reference"
        elif is_formula:
            reason = "matched" if equivalent(value, saved_value) else "different"
        else:
            reason = "frozen" if equivalent(value, saved_value) else "frozen_different"
        counts[reason] += 1
        if reason in ("missing", "different", "frozen_different") and len(examples) < 8:
            examples.append((address, value, saved_value))
    counts["extra"] = len(actual.keys() - expected.keys())
    failures = ("different", "frozen", "frozen_different", "missing", "extra", "uncached_reference")
    summary = " ".join(f"{key}={counts[key]}" for key in ("matched", *failures))
    print(f"{result} {summary}")
    for address, before, after in examples:
        print(f"  {address!r} {before!r} -> {after!r}")
    return counts["matched"] > 0 and not any(counts[key] for key in failures)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reference")
    parser.add_argument("results", nargs="+")
    args = parser.parse_args()
    matches = [compare(args.reference, result) for result in args.results]
    return 0 if all(matches) else 1


if __name__ == "__main__":
    sys.exit(main())
