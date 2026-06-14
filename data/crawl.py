from pathlib import Path
from pprint import pprint  # noqa: F401
import re
import json

import requests
import typer

cli = typer.Typer(pretty_exceptions_show_locals=False)

model_dir = Path(__file__).parent
error_codes_path = model_dir / "s3_error_codes.json"


def save_json(path, data):
    with open(path, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=4)


def load_json(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def download_aws_sdk(service: str, *, commit: str):
    url = f"https://github.com/awslabs/aws-sdk-rust/raw/{commit}/aws-models/{service}.json"
    resp = requests.get(url)
    assert resp.status_code == 200
    assert resp.json()
    with open(model_dir / f"{service}.json", "w") as f:
        f.write(resp.text)


@cli.command()
def download_s3_model():
    # https://github.com/awslabs/aws-sdk-rust/commits/main/aws-models/s3.json
    download_aws_sdk("s3", commit="2c2a06e583392266669e075d4a47489d6da1e055")


@cli.command()
def download_sts_model():
    # https://github.com/awslabs/aws-sdk-rust/commits/main/aws-models/sts.json
    download_aws_sdk("sts", commit="13eb310a6cbb4912f0a44db2fb2fca0b2bfee5d1")


@cli.command()
def crawl_error_codes():
    md_url = "https://docs.aws.amazon.com/AmazonS3/latest/API/API_Error.md"

    try:
        md_text = requests.get(md_url).text
    except requests.RequestException as e:
        raise RuntimeError(f"unable to fetch {md_url}") from e

    if md_text is None or len(md_text) < 100:
        raise RuntimeError(
            f"unexpected response from {md_url} (len={len(md_text) if md_text else 0})"
        )

    data = crawl_error_codes_from_markdown(md_text)
    if data is None:
        raise RuntimeError("unable to parse S3 error code docs")

    # Merge with existing data:
    #   - For sections not covered by the new docs, preserve the old data as-is.
    #   - Within the "S3" section, prefer the new description unless the old
    #     one is substantially more detailed (>=3x longer), in which case the
    #     new markdown likely lost important content.
    if error_codes_path.exists():
        old_data = load_json(error_codes_path)
        for section, old_entries in old_data.items():
            if section not in data:
                data[section] = old_entries
            elif section == "S3":
                old_map = {e["code"]: e for e in old_entries}
                new_codes = {e["code"] for e in data["S3"]}
                for old_entry in old_entries:
                    if old_entry["code"] not in new_codes:
                        data["S3"].append(old_entry)
                for entry in data["S3"]:
                    old_entry = old_map.get(entry["code"])
                    if old_entry is None:
                        continue
                    old_desc = old_entry["description"]
                    new_desc = entry["description"]
                    # Keep old description if it is significantly more detailed
                    if len(old_desc) >= 50 and len(old_desc) >= len(new_desc) * 3:
                        entry["description"] = old_desc
                data["S3"].sort(key=lambda e: e["code"])

    save_json(error_codes_path, data)


def crawl_error_codes_from_markdown(md_text: str):
    """Parse error codes from AWS API_Error.md markdown.

    The error code list uses a definition-list-like structure::

        +
          +  *Code:* AccessDenied
          +  *Description:* Access Denied
          +  *HTTP Status Code:* 403 Forbidden
          +  *SOAP Fault Code Prefix:* Client

    Some entries have a known formatting bug where ``*Code:*`` is used
    instead of ``*HTTP Status Code:*`` for the status line.  The first
    ``*Code:*`` field is always the error code name; every subsequent one
    is treated as a status line.
    """
    entries: dict[str, dict] = {}

    # Split by entry delimiters: lines that are exactly '+' (with optional trailing whitespace)
    blocks = re.split(r"\n\+[ \t]*\n", md_text)

    for block in blocks:
        code: str | None = None
        description: str | None = None
        http_status_raw: str | None = None

        lines = block.strip().split("\n")
        code_count = 0

        for line in lines:
            line = line.strip()

            m = re.match(r"\+\s+\*Code:\*\s*(.+)", line)
            if m:
                code_count += 1
                if code_count == 1:
                    code = m.group(1).strip()
                else:
                    # Subsequent *Code:* lines are actually HTTP status codes (AWS docs bug)
                    if http_status_raw is None:
                        val = m.group(1).strip()
                        if val != "N/A":
                            http_status_raw = val
                continue

            m = re.match(r"\+\s+\*Description:\*\s*(.+)", line)
            if m:
                description = m.group(1).strip()
                continue

            m = re.match(r"\+\s+\*HTTP Status Code:\*\s*(.+)", line)
            if m:
                val = m.group(1).strip()
                if http_status_raw is None and val != "N/A":
                    http_status_raw = val
                continue

        if code and description:
            if code not in entries:
                entries[code] = {
                    "description": description,
                    "http_status_raw": http_status_raw,
                }

    if not entries:
        return None

    ans: list[dict] = []
    for code, info in sorted(entries.items()):
        http_status_code: int | None = None
        if info["http_status_raw"]:
            m = re.match(r"(\d{3})", info["http_status_raw"])
            if m:
                http_status_code = int(m.group(1))

        ans.append(
            {
                "code": code,
                "description": _clean_description(info["description"]),
                "http_status_code": http_status_code,
            }
        )

    return {"S3": ans}


def _clean_description(desc: str) -> str:
    """Strip markdown link syntax ``[text](url)`` → ``text``."""
    return re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", desc)


@cli.command()
def download_date_time_format_test_suite():
    # https://github.com/smithy-lang/smithy-rs/blob/main/rust-runtime/aws-smithy-types/test_data/date_time_format_test_suite.json
    url = "https://github.com/smithy-lang/smithy-rs/raw/main/rust-runtime/aws-smithy-types/test_data/date_time_format_test_suite.json"
    resp = requests.get(url)
    assert resp.status_code == 200
    assert resp.json()
    with open(model_dir / "date_time_format_test_suite.json", "w") as f:
        f.write(resp.text)


@cli.command()
def update():
    download_s3_model()
    download_sts_model()
    crawl_error_codes()
    download_date_time_format_test_suite()


if __name__ == "__main__":
    cli()
