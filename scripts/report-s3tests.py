#!/usr/bin/env python3
import sys
from xml.etree import ElementTree


def parse_report(report_path: str) -> ElementTree.Element:
    try:
        tree = ElementTree.parse(report_path)
    except FileNotFoundError as exc:
        raise SystemExit(f"report not found: {report_path}") from exc
    except ElementTree.ParseError as exc:
        raise SystemExit(f"error parsing {report_path}: {exc}") from exc
    return tree.getroot()


def get_int_attr(elem: ElementTree.Element, name: str) -> int:
    value = elem.attrib.get(name, "0")
    try:
        return int(float(value))
    except ValueError as exc:
        raise SystemExit(f"invalid {name} value: {value}") from exc


def summarize(report_path: str) -> None:
    root = parse_report(report_path)
    suites = root.findall("testsuite")
    if root.tag == "testsuite":
        suites.append(root)

    tests = sum(get_int_attr(suite, "tests") for suite in suites)
    failures = sum(get_int_attr(suite, "failures") for suite in suites)
    errors = sum(get_int_attr(suite, "errors") for suite in suites)
    skipped = sum(get_int_attr(suite, "skipped") for suite in suites)

    print(f"tests {tests}, failures {failures}, errors {errors}, skipped {skipped}")

    if failures or errors:
        raise SystemExit("s3-tests reported failures")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: report-s3tests.py <junit.xml>")
    summarize(sys.argv[1])
