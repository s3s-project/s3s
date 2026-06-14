import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch


spec = importlib.util.spec_from_file_location(
    "crawl", Path(__file__).with_name("crawl.py")
)
assert spec is not None
assert spec.loader is not None
crawl = importlib.util.module_from_spec(spec)
spec.loader.exec_module(crawl)

UNEXPECTED_AWS_PAGE = "<html><head><title>Amazon S3</title></head><body></body></html>"


class CrawlErrorCodesTestCase(unittest.TestCase):
    def test_crawl_error_codes_from_html_returns_none_for_unexpected_page(self):
        self.assertIsNone(crawl.crawl_error_codes_from_html(UNEXPECTED_AWS_PAGE))

    def test_crawl_error_codes_keeps_existing_data_when_parsing_fails(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            error_codes_path = Path(tmpdir) / "s3_error_codes.json"
            expected = {
                "S3": [
                    {
                        "code": "SlowDown",
                        "description": "Slow Down",
                        "http_status_code": 503,
                    }
                ]
            }
            error_codes_path.write_text(json.dumps(expected))

            with (
                patch.object(crawl, "error_codes_path", error_codes_path),
                patch.object(
                    crawl.requests,
                    "get",
                    return_value=SimpleNamespace(text=UNEXPECTED_AWS_PAGE),
                ) as mock_get,
                patch.object(crawl.typer, "echo") as mock_echo,
            ):
                crawl.crawl_error_codes()

            self.assertEqual(crawl.load_json(error_codes_path), expected)
            self.assertEqual(mock_get.call_count, 2)
            mock_echo.assert_called_once()


if __name__ == "__main__":
    unittest.main()
