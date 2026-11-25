import unittest

from intervals import merge_intervals


class MergeTest(unittest.TestCase):
    def test_overlapping(self):
        self.assertEqual(merge_intervals([(1, 4), (2, 6)]), [(1, 6)])

    def test_separate(self):
        self.assertEqual(merge_intervals([(1, 2), (5, 6)]), [(1, 2), (5, 6)])

    def test_empty(self):
        self.assertEqual(merge_intervals([]), [])


if __name__ == "__main__":
    unittest.main()
