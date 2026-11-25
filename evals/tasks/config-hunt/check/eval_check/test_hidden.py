import json
import pathlib
import re
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent


def constant(relative, name):
    source = (ROOT / relative).read_text(encoding="utf-8")
    match = re.search(rf"^{name}\s*=\s*(\d+)", source, re.MULTILINE)
    if match is None:
        raise AssertionError(f"{name} is no longer set in {relative}")
    return int(match.group(1))


class Hidden(unittest.TestCase):
    def test_http_timeouts_are_45(self):
        self.assertEqual(constant("service/net/client.py", "REQUEST_TIMEOUT_S"), 45)
        self.assertEqual(constant("service/net/session.py", "DEFAULT_TIMEOUT"), 45)
        config = json.loads((ROOT / "config/defaults.json").read_text(encoding="utf-8"))
        self.assertEqual(config["http"]["request_timeout_s"], 45)

    def test_look_alikes_are_untouched(self):
        self.assertEqual(constant("service/net/retry.py", "RETRY_DELAY_S"), 30)
        self.assertEqual(constant("service/net/session.py", "CACHE_TTL_S"), 30)
        self.assertEqual(constant("service/db.py", "DB_QUERY_TIMEOUT_S"), 30)
        self.assertEqual(constant("service/db.py", "PAGE_SIZE"), 30)
        config = json.loads((ROOT / "config/defaults.json").read_text(encoding="utf-8"))
        self.assertEqual(config["http"]["max_connections"], 30)
        self.assertEqual(config["database"]["query_timeout_s"], 30)
        self.assertEqual(config["cache"]["ttl_s"], 30)

    def test_the_client_still_uses_its_constant(self):
        source = (ROOT / "service/net/client.py").read_text(encoding="utf-8")
        self.assertIn("timeout=REQUEST_TIMEOUT_S", source)


if __name__ == "__main__":
    unittest.main()
