import unittest

from text_utils import slugify


class Hidden(unittest.TestCase):
    def test_basics(self):
        self.assertEqual(slugify("Hello, World!"), "hello-world")
        self.assertEqual(slugify("  Crème Brûlée  "), "creme-brulee")
        self.assertEqual(slugify("a--b__c"), "a-b-c")
        self.assertEqual(slugify("Version 2.0 Release"), "version-2-0-release")
        self.assertEqual(slugify("日本語 text"), "text")
        self.assertEqual(slugify("Ça / Zürich"), "ca-zurich")

    def test_empty(self):
        self.assertEqual(slugify(""), "")
        self.assertEqual(slugify("!!!"), "")
        self.assertEqual(slugify("日本語"), "")

    def test_max_length(self):
        self.assertEqual(slugify("The quick brown fox", max_length=13), "the-quick")
        self.assertEqual(slugify("The quick brown fox", max_length=9), "the-quick")
        self.assertEqual(slugify("The quick brown fox", max_length=10), "the-quick")
        self.assertEqual(slugify("The quick brown fox", max_length=15), "the-quick-brown")
        self.assertEqual(slugify("The quick brown fox", max_length=100), "the-quick-brown-fox")
        self.assertEqual(slugify("Supercalifragilistic", max_length=5), "super")
        self.assertEqual(slugify("ab cd", max_length=5), "ab-cd")


if __name__ == "__main__":
    unittest.main()
