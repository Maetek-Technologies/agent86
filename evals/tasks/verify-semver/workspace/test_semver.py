import unittest

from semver import compare_versions


class CompareTest(unittest.TestCase):
    def test_core(self):
        self.assertEqual(compare_versions("1.0.0", "2.0.0"), -1)
        self.assertEqual(compare_versions("1.10.0", "1.9.0"), 1)

    def test_prerelease(self):
        self.assertEqual(compare_versions("1.0.0-alpha", "1.0.0"), -1)

    def test_equal(self):
        self.assertEqual(compare_versions("1.2.3", "1.2.3"), 0)


if __name__ == "__main__":
    unittest.main()
