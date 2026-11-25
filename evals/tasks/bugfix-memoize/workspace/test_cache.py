import unittest

from cache import memoize


class MemoizeTest(unittest.TestCase):
    def test_same_call_is_cached(self):
        calls = []

        @memoize
        def double(x):
            calls.append(x)
            return x * 2

        self.assertEqual(double(3), 6)
        self.assertEqual(double(3), 6)
        self.assertEqual(calls, [3])

    def test_keywords_are_part_of_the_key(self):
        @memoize
        def scaled(x, scale=1):
            return x * scale

        self.assertEqual(scaled(5), 5)
        self.assertEqual(scaled(5, scale=3), 15)


if __name__ == "__main__":
    unittest.main()
