import logging
import pathlib
import unittest

from stats import median, smooth

ROOT = pathlib.Path(__file__).resolve().parent.parent


class Hidden(unittest.TestCase):
    def test_median(self):
        self.assertEqual(median([4, 1, 3, 2]), 2.5)
        self.assertEqual(median([1, 2]), 1.5)
        self.assertEqual(median([7]), 7)
        self.assertEqual(median([5, 1, 3]), 3)
        self.assertEqual(median([1.5, 2.5, 10.0, -1.0]), 2.0)
        with self.assertRaises(ValueError):
            median([])

    def test_smooth(self):
        logging.disable(logging.CRITICAL)
        try:
            self.assertEqual(smooth([1, 9, 2, 8]), [1, 5.0, 2, 8])
        finally:
            logging.disable(logging.NOTSET)

    def test_the_suite_passes(self):
        logging.disable(logging.CRITICAL)
        try:
            suite = unittest.defaultTestLoader.discover(str(ROOT / "tests"), top_level_dir=str(ROOT))
            result = unittest.TestResult()
            suite.run(result)
        finally:
            logging.disable(logging.NOTSET)
        self.assertEqual(result.testsRun, 6)
        self.assertEqual(result.failures + result.errors, [])


if __name__ == "__main__":
    unittest.main()
