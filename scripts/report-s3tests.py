#!/usr/bin/env python3
import re
import sys
from collections import defaultdict
from xml.etree import ElementTree

# Baseline results as of 2026-02-08; reduce these as compatibility improves.
ALLOWED_FAILURES = 326
ALLOWED_ERRORS = 302

# Mapping from the last component of the pytest classname to an S3 capability
# category.  Tests in s3tests.functional.test_s3 are further classified by
# keywords found in the test name (see _S3_PATTERNS below).
_MODULE_CATEGORIES = {
    "test_headers": "headers",
    "test_iam": "iam",
    "test_s3select": "s3-select",
    "test_sns": "sns",
    "test_sts": "sts",
    "test_utils": "utils",
}

# Ordered list of (compiled pattern, category) pairs used to classify tests
# from s3tests.functional.test_s3.  The first matching pattern wins.
_S3_PATTERNS = [
    (re.compile(r"object_lock"), "object-lock"),
    (re.compile(r"presigned"), "presigned"),
    (re.compile(r"multipart"), "multipart"),
    (re.compile(r"sse_|_sse_|sse$|encryption|encrypted|_enc_|_kms_"), "encryption"),
    (re.compile(r"versioning|versioned|delete_marker"), "versioning"),
    (re.compile(r"lifecycle"), "lifecycle"),
    (re.compile(r"cors"), "cors"),
    (re.compile(r"post_object"), "post-object"),
    (re.compile(r"_tag"), "tagging"),
    (re.compile(r"acl"), "acl"),
    (re.compile(r"access_bucket|block_public|ignore_public|public_block"), "acl"),
    (re.compile(r"_policy|policy_"), "policy"),
    (re.compile(r"bucket_list|listv2"), "list-objects"),
    (re.compile(r"logging"), "logging"),
    (re.compile(r"delete"), "delete"),
    (re.compile(r"restore"), "restore"),
    (re.compile(r"ranged"), "range-get"),
    (re.compile(r"copy"), "copy"),
    (re.compile(r"bucket"), "bucket"),
]


def classify_test(classname: str, name: str) -> str:
    """Return the S3 capability category for a test case."""
    module = classname.rsplit(".", 1)[-1]
    if module != "test_s3":
        return _MODULE_CATEGORIES.get(module, "other")
    for pattern, category in _S3_PATTERNS:
        if pattern.search(name):
            return category
    return "object"


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
        return int(value)
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

    # Per-capability breakdown
    counts = defaultdict(
        lambda: {"total": 0, "passed": 0, "failures": 0, "errors": 0, "skipped": 0}
    )
    for suite in suites:
        for tc in suite.iter("testcase"):
            classname = tc.attrib.get("classname", "")
            name = tc.attrib.get("name", "")
            cat = classify_test(classname, name)
            counts[cat]["total"] += 1
            if tc.find("failure") is not None:
                counts[cat]["failures"] += 1
            elif tc.find("error") is not None:
                counts[cat]["errors"] += 1
            elif tc.find("skipped") is not None:
                counts[cat]["skipped"] += 1
            else:
                counts[cat]["passed"] += 1

    print()
    print(
        f"{'capability':<16} {'total':>6} {'passed':>7} {'failures':>9} {'errors':>7} {'skipped':>8}"
    )
    for cat, s in sorted(counts.items()):
        print(
            f"{cat:<16} {s['total']:>6} {s['passed']:>7} {s['failures']:>9} {s['errors']:>7} {s['skipped']:>8}"
        )

    if failures > ALLOWED_FAILURES or errors > ALLOWED_ERRORS:
        raise SystemExit(
            "s3-tests regressions: "
            f"failures {failures} (allowed {ALLOWED_FAILURES}), "
            f"errors {errors} (allowed {ALLOWED_ERRORS})"
        )


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: report-s3tests.py <junit.xml>")
    summarize(sys.argv[1])
