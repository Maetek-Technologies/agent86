import itertools
import unittest

from semver import compare_versions

# The precedence example from the Semantic Versioning 2.0.0 specification, plus
# a few more, lowest first.
ORDERED = [
    "0.9.9", "1.0.0-0", "1.0.0-0.0", "1.0.0-1", "1.0.0-2", "1.0.0-10", "1.0.0-0A",
    "1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-alpha-1", "1.0.0-beta",
    "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0", "1.0.1", "1.1.0", "1.10.0", "2.0.0",
]


class Hidden(unittest.TestCase):
    def test_total_order(self):
        for (i, a), (j, b) in itertools.product(enumerate(ORDERED), repeat=2):
            expected = (i > j) - (i < j)
            self.assertEqual(compare_versions(a, b), expected, f"{a} vs {b}")

    def test_build_metadata_is_ignored(self):
        self.assertEqual(compare_versions("1.0.0+a", "1.0.0+b"), 0)
        self.assertEqual(compare_versions("1.0.0-alpha+001", "1.0.0-alpha"), 0)
        self.assertEqual(compare_versions("1.0.0+20130313144700", "1.0.0"), 0)
        self.assertEqual(compare_versions("1.0.0-beta+exp.sha.5114f85", "1.0.0-alpha"), 1)

    def test_invalid(self):
        for bad in ("1.0", "1.0.0.0", "01.0.0", "1.02.0", "1.0.0-", "1.0.0-alpha..1", "1.0.0-01",
                    "1.0.0+", "1.0.0-al$pha", "v1.0.0", " 1.0.0", "1.0.0-alpha_beta", "", "1.0.0+a..b",
                    "-1.0.0", "1.0.x"):
            with self.assertRaises(ValueError, msg=repr(bad)):
                compare_versions(bad, "1.0.0")
            with self.assertRaises(ValueError, msg=repr(bad)):
                compare_versions("1.0.0", bad)

    def test_valid_corners(self):
        self.assertEqual(compare_versions("1.0.0+001", "1.0.0"), 0)
        self.assertEqual(compare_versions("0.0.0", "0.0.0-x"), 1)
        self.assertEqual(compare_versions("1.0.0-x-y-z.--", "1.0.0-x-y-z.--"), 0)


if __name__ == "__main__":
    unittest.main()
