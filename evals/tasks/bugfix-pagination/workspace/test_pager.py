import unittest

from pager import paginate


class PaginateTest(unittest.TestCase):
    def test_first_page(self):
        self.assertEqual(paginate(list(range(25)), 1), list(range(10)))

    def test_second_page(self):
        self.assertEqual(paginate(list(range(25)), 2, per_page=10), list(range(10, 20)))


if __name__ == "__main__":
    unittest.main()
