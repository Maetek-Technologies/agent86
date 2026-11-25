import random
import unittest

from intervals import merge_intervals


def covered(intervals):
    points = set()
    for start, end in intervals:
        points.update(range(start, end + 1))
    return points


class Hidden(unittest.TestCase):
    def test_cases(self):
        self.assertEqual(merge_intervals([(1, 4), (2, 6)]), [(1, 6)])
        self.assertEqual(merge_intervals([(5, 6), (1, 2)]), [(1, 2), (5, 6)])
        self.assertEqual(merge_intervals([(1, 3), (4, 6)]), [(1, 6)])
        self.assertEqual(merge_intervals([(1, 3), (3, 4)]), [(1, 4)])
        self.assertEqual(merge_intervals([(1, 3), (5, 6)]), [(1, 3), (5, 6)])
        self.assertEqual(merge_intervals([(1, 10), (2, 3), (4, 5)]), [(1, 10)])
        self.assertEqual(merge_intervals([(7, 7)]), [(7, 7)])
        self.assertEqual(merge_intervals([(-5, -3), (-2, 0)]), [(-5, 0)])
        self.assertEqual(merge_intervals([(3, 4), (1, 1), (2, 2)]), [(1, 4)])
        self.assertEqual(merge_intervals([]), [])
        self.assertEqual(merge_intervals(iter([(2, 3), (0, 1)])), [(0, 3)])

    def test_result_shape_and_input(self):
        data = [[4, 5], [1, 2]]
        result = merge_intervals(data)
        self.assertEqual(data, [[4, 5], [1, 2]])
        self.assertEqual(result, [(1, 2), (4, 5)])
        self.assertTrue(all(isinstance(pair, tuple) for pair in result))

    def test_errors(self):
        for bad in ([(3, 1)], [(1, 2), (5, 4)], [(1.0, 2)], [(1, "2")], [(True, 2)], [(1, None)]):
            with self.assertRaises(ValueError, msg=repr(bad)):
                merge_intervals(bad)

    def test_random(self):
        rng = random.Random(86)
        for _ in range(500):
            intervals = []
            for _ in range(rng.randint(0, 8)):
                start = rng.randint(-20, 20)
                intervals.append((start, start + rng.randint(0, 6)))
            result = merge_intervals(intervals)
            self.assertEqual(covered(result), covered(intervals))
            for (_, end), (start, _) in zip(result, result[1:]):
                self.assertGreater(start, end + 1)


if __name__ == "__main__":
    unittest.main()
