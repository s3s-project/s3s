from pathlib import Path
from pprint import pprint  # noqa: F401
import re
import json

from bs4 import BeautifulSoup
import requests
import typer

cli = typer.Typer(pretty_exceptions_show_locals=False)

model_dir = Path(__file__).parent
error_codes_path = model_dir / "s3_error_codes.json"


def save_json(path, data):
    with open(path, "w") as f:
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
    urls = (
        "https://docs.aws.amazon.com/AmazonS3/latest/API/ErrorResponses.html",
        "https://docs.aws.amazon.com/AmazonS3/latest/API/API_Error.html",
    )

    data = None
    for url in urls:
        html = requests.get(url).text
        data = crawl_error_codes_from_html(html)
        if data is not None:
            break

    if data is None:
        if error_codes_path.exists():
            typer.echo(
                "warning: unable to parse S3 error code docs; keeping existing data"
            )
            return
        raise RuntimeError(
            "unable to parse S3 error code docs and no existing data is available"
        )

    save_json(error_codes_path, data)


def crawl_error_codes_from_html(html):
    soup = BeautifulSoup(html, "lxml")

    kinds = [
        ("S3", "ErrorCodeList"),
        ("Replication", "ReplicationErrorCodeList"),
        ("Tagging", "S3TaggingErrorCodeList"),
        ("SelectObjectContent", "SelectObjectContentErrorCodeList"),
    ]

    data = {}

    for kind, h2_id in kinds:
        h2_list = soup.css.select(f"#{h2_id}")  # type:ignore
        if not h2_list:
            return None
        h2 = h2_list[0]

        # find the next table
        table = None
        for e in h2.next_elements:
            if e.name == "table":  # type:ignore
                table = e
                break
        if table is None:
            return None

        th_list = table.css.select("th")  # type:ignore
        if len(th_list) < 3:
            return None
        if th_list[0].text not in ("Error code", "Error Code"):
            return None
        if th_list[1].text != "Description":
            return None
        if th_list[2].text not in ("HTTP status code", "HTTP Status Code"):
            return None

        tr_list = table.css.select("tr")[1:]  # type:ignore
        tr_list = [[e for e in tr.children if e.name == "td"] for tr in tr_list]

        ans = []
        for td_list in tr_list:
            if len(td_list) < 3:
                continue
            td0_code = td_list[0].css.select("code")
            if td0_code:
                t0 = td0_code[0].text.strip()
            else:
                t0 = td_list[0].text.strip()

            t1 = td_list[1].text.strip()
            t2 = td_list[2].text.strip()

            error_code = t0

            description = re.sub(r"\n\t+", " ", t1).strip()

            if t2 == "N/A":
                http_status_code = None
            else:
                m = re.match(r"(\d{3})[\s\S]*", t2)
                if m is None:
                    continue  # FIXME: EntityTooLarge 405
                # assert m is not None, f"t2: {repr(t2)}"
                http_status_code = int(m.group(1))

            ans.append(
                {
                    "code": error_code,
                    "description": description,
                    "http_status_code": http_status_code,
                }
            )

        ans.sort(key=lambda x: x["code"])
        data[kind] = ans

    return data


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
