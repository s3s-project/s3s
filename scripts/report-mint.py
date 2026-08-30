#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2023-2026 The s3s Authors

import json
import re
import sys
from dataclasses import dataclass
from itertools import groupby
from pprint import pprint  # noqa: F401
from typing import Any


# https://github.com/minio/mint#mint-log-format
@dataclass
class MintLog:
    name: str
    function: str | None
    args: dict[str, Any] | None
    duration: int
    status: str
    alert: str | None
    message: str | None
    error: str | None


def from_json(x: Any) -> MintLog:
    return MintLog(
        name=x["name"],
        function=x.get("function"),
        args=x.get("args"),
        duration=x["duration"],
        status=x["status"],
        alert=x.get("alert"),
        message=x.get("message"),
        error=x.get("error"),
    )


# Per-function gate: only the test functions listed here are allowed to fail,
# and each entry caps how many times it may fail. Any failure outside the
# list fails the gate, and every entry must actually run in this mint log
# (a stale entry after a mint image upgrade is reported instead of being
# silently ignored).
#
# Baselines recorded on 2026-08-30 against minio/mint:edge and
# minio/minio:latest; each entry maps to a tracked known issue.
EXPECTED_FAILURES: dict[str, dict[str, int]] = {
    "aws-sdk-go-v2": {
        # FIXME: https://github.com/minio/mint/blob/master/run/core/aws-sdk-go-v2/main.go#L294
        "ConditionalDeleteWithIncorrectETag": 1,
    },
    "aws-sdk-ruby": {
        "presignedPost(bucket_name,file_name,expires_in_sec,max_byte_size)": 1,
    },
    "healthcheck": {
        "testLivenessEndpoint": 1,
    },
    "mc": {
        "test_admin_users": 1,
    },
    "minio-java": {
        "getObjectAcl()": 1,
    },
    "minio-js": {
        "copyObject(bucketName, objectName, srcObject, conditions, cb)": 1,
        "listObjects(bucketName, prefix, recursive)": 3,
        "extensions.listObjectsV2WithMetadata(bucketName, prefix, recursive)": 1,
        "Put an object with assume role credentials:  bucket:": 1,
        '"after all" hook in "Force Deletion of objects with versions"': 1,
        '"after all" hook in "Force Deletion of prefix with versions"': 1,
        '"after all" hook in "Force Deletion of prefix"': 1,
        '"after all" hook in "functional tests"': 1,
    },
}

# The awscli runner uses the full command line as the test function name,
# including a random bucket name; normalize it so the name is stable across
# runs.
_AWS_CLI_BUCKET_PATTERN = re.compile(r"awscli-mint-test-bucket-\d+")


def normalize_function(name: str, function: str) -> str:
    if name == "awscli":
        return _AWS_CLI_BUCKET_PATTERN.sub("awscli-mint-test-bucket-N", function)
    return function


def check_counters(counts: dict[str, dict[str, int]]) -> list[str]:
    """Evaluate the group-level counter assertions.

    Returns a list of violations; an empty list means all counters are fine.
    """
    errors: list[str] = []

    def check_pass_at_least(name: str, minimum: int) -> None:
        pass_count = counts[name]["pass"]
        if pass_count < minimum:
            errors.append(
                f'group counter: "{name}" passed {pass_count}, expected at least {minimum}'
            )

    def check_fail_zero(name: str) -> None:
        fail_count = counts[name]["fail"]
        if fail_count != 0:
            errors.append(
                f'group counter: "{name}" failed {fail_count} test(s), expected 0'
            )

    check_pass_at_least("aws-sdk-go-v2", 5)
    check_fail_zero("aws-sdk-php")
    check_pass_at_least("aws-sdk-ruby", 12)
    check_fail_zero("awscli")
    check_pass_at_least("mc", 16)
    check_fail_zero("minio-go")
    check_pass_at_least("minio-java", 43)
    check_pass_at_least("minio-js", 190)
    check_pass_at_least("minio-py", 16)
    check_fail_zero("s3cmd")
    check_fail_zero("s3select")
    check_pass_at_least("versioning", 4)

    return errors


def check_gate(logs: list[MintLog]) -> list[str]:
    """Evaluate the per-function gate.

    Returns a list of gate violations; an empty list means the gate passed.
    """
    errors: list[str] = []

    fail_counts: dict[tuple[str, str], int] = {}
    appearances: set[tuple[str, str]] = set()
    for x in logs:
        key = (x.name, normalize_function(x.name, x.function or ""))
        appearances.add(key)
        if x.status == "FAIL":
            fail_counts[key] = fail_counts.get(key, 0) + 1

    for name, functions in EXPECTED_FAILURES.items():
        for function, max_fail in functions.items():
            key = (name, function)
            if key not in appearances:
                errors.append(
                    f'expected failure entry is stale: "{name}" "{function}" did not run'
                )
                continue
            fail_count = fail_counts.get(key, 0)
            if fail_count > max_fail:
                errors.append(
                    f'"{name}" "{function}" failed {fail_count} time(s), expected at most {max_fail}'
                )

    for (name, function), fail_count in fail_counts.items():
        if function not in EXPECTED_FAILURES.get(name, {}):
            errors.append(
                f'unexpected failure: "{name}" "{function}" failed {fail_count} time(s)'
            )

    return errors


if __name__ == "__main__":
    log_path = sys.argv[1]
    logs = []
    with open(log_path) as f:
        for line in f:
            line = line.strip()
            if len(line) == 0:
                continue

            json_str = line
            if json_str.find("{") != 0:
                json_str = json_str[json_str.find("{") :]

            try:
                json_value = json.loads(json_str)
            except json.JSONDecodeError:
                print(f"error parsing log line: {line}")
                continue

            logs.append(from_json(json_value))

    for x in logs:
        if ":" in x.name:
            name, function = x.name.split(":")
            x.name = name.strip()
            x.function = function.strip()

    groups = {k: list(v) for k, v in groupby(logs, lambda x: x.name)}
    counts = {}

    for name, group in groups.items():
        pass_count = sum(1 for x in group if x.status == "PASS")
        fail_count = sum(1 for x in group if x.status == "FAIL")
        na_count = sum(1 for x in group if x.status == "NA")
        counts[name] = {"pass": pass_count, "fail": fail_count, "na": na_count}

        print(
            f"{name:<20} "
            f"passed {pass_count:>3}, "
            f"failed {fail_count:>3}, "
            f"na {na_count:>3}"
        )
    print()

    total_pass_count = sum(c["pass"] for c in counts.values())
    total_fail_count = sum(c["fail"] for c in counts.values())
    total_na_count = sum(c["na"] for c in counts.values())
    name = "summary"
    print(
        f"{name:<20} "
        f"passed {total_pass_count:>3}, "
        f"failed {total_fail_count:>3}, "
        f"na {total_na_count:>3}"
    )

    # Both gates run to completion so every violation is reported; the exit
    # code is decided afterwards.
    errors = check_counters(counts)
    errors += check_gate(logs)
    if errors:
        print()
        print("mint gate check failed:")
        for error in errors:
            print(f"  - {error}")
        sys.exit(1)
