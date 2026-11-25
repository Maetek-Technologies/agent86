import unittest

from pager import page_count, paginate


class Hidden(unittest.TestCase):
    def test_pages(self):
        items = list(range(25))
        self.assertEqual(paginate(items, 1), list(range(10)))
        self.assertEqual(paginate(items, 3), [20, 21, 22, 23, 24])
        self.assertEqual(paginate(items, 4), [])
        self.assertEqual(paginate(items, 2, per_page=7), list(range(7, 14)))
        self.assertEqual(paginate([], 1), [])

    def test_bad_arguments(self):
        with self.assertRaises(ValueError):
            paginate([1], 0)
        with self.assertRaises(ValueError):
            paginate([1], 1, per_page=0)
        with self.assertRaises(ValueError):
            page_count(5, per_page=0)

    def test_page_count(self):
        self.assertEqual(page_count(25), 3)
        self.assertEqual(page_count(20), 2)
        self.assertEqual(page_count(1), 1)
        self.assertEqual(page_count(0), 0)
        self.assertEqual(page_count(7, per_page=3), 3)


if __name__ == "__main__":
    unittest.main()
