import logging
import random
import unittest

from stats import mean, median, smooth

# The pipeline's logging, as it runs in production.
logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s")


def readings(seed, count=1500):
    rng = random.Random(seed)
    return [round(rng.uniform(10, 30), 2) for _ in range(count)]


class StatsTest(unittest.TestCase):
    def test_mean(self):
        smooth(readings(1))
        self.assertEqual(mean([1, 2, 3, 4]), 2.5)

    def test_median_odd(self):
        smooth(readings(2))
        self.assertEqual(median([5, 1, 3]), 3)

    def test_smooth_length(self):
        data = readings(3)
        self.assertEqual(len(smooth(data)), len(data))

    def test_smooth_is_bounded(self):
        data = readings(4)
        smoothed = smooth(data)
        self.assertTrue(all(min(data) <= value <= max(data) for value in smoothed))

    def test_median_even(self):
        smooth(readings(5))
        self.assertEqual(median([4, 1, 3, 2]), 2.5)

    def test_median_empty(self):
        smooth(readings(6))
        with self.assertRaises(ValueError):
            median([])


if __name__ == "__main__":
    unittest.main()
