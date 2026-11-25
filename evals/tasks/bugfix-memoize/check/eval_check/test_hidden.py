import unittest

from cache import memoize


def counted():
    calls = []

    @memoize
    def add(a, b=0, *, scale=1):
        calls.append((a, b, scale))
        return (a + b) * scale

    return add, calls


class Hidden(unittest.TestCase):
    def test_keys(self):
        add, calls = counted()
        self.assertEqual(add(1), 1)
        self.assertEqual(add(1, scale=2), 2)
        self.assertEqual(add(1, b=2), 3)
        self.assertEqual(add(1, b=2, scale=3), 9)
        self.assertEqual(add(1, scale=3, b=2), 9)
        self.assertEqual(len(calls), 4)
        self.assertEqual(add.cache_size(), 4)

    def test_clear(self):
        add, calls = counted()
        add(2)
        add(2)
        self.assertEqual(add.cache_size(), 1)
        add.cache_clear()
        self.assertEqual(add.cache_size(), 0)
        add(2)
        self.assertEqual(len(calls), 2)

    def test_unhashable(self):
        add, _ = counted()
        with self.assertRaises(TypeError):
            add([1], b=[2])

    def test_wraps(self):
        add, _ = counted()
        self.assertEqual(add.__name__, "add")


if __name__ == "__main__":
    unittest.main()
