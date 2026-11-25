import pathlib
import unittest
import warnings

import inventory
from inventory import Order, monthly_report
from inventory import pricing

ROOT = pathlib.Path(__file__).resolve().parent.parent


class Hidden(unittest.TestCase):
    def test_new_name_is_exported(self):
        from inventory import order_total

        self.assertIn("order_total", inventory.__all__)
        self.assertEqual(order_total([(10.0, 2), (5.0, 1)], tax_rate=0.1), 27.5)
        self.assertEqual(order_total([(10.0, 1)], discount=0.25), 7.5)

    def test_alias_warns_and_agrees(self):
        with warnings.catch_warnings(record=True) as caught:
            warnings.simplefilter("always")
            value = pricing.calc_total([(10.0, 2)], tax_rate=0.5)
        self.assertEqual(value, 30.0)
        self.assertTrue(any(issubclass(w.category, DeprecationWarning) for w in caught))

    def test_callers_use_the_new_name(self):
        for name in ("orders.py", "reports.py"):
            source = (ROOT / "inventory" / name).read_text(encoding="utf-8")
            self.assertNotIn("calc_total", source, name)
        with warnings.catch_warnings(record=True) as caught:
            warnings.simplefilter("always")
            order = Order("ana", [(10.0, 1)], tax_rate=0.2)
            self.assertEqual(order.add(5.0, 2), 24.0)
            self.assertEqual(order.total(discount=0.5), 12.0)
            report = monthly_report([Order("a", [(1.0, 3)]), Order("b", [(4.0, 1)])])
        self.assertEqual(report, {"revenue": 7.0, "largest": 4.0, "count": 2})
        self.assertFalse(
            [w for w in caught if issubclass(w.category, DeprecationWarning)],
            "the package's own code still calls the deprecated alias",
        )

    def test_the_existing_tests_pass(self):
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            suite = unittest.defaultTestLoader.discover(str(ROOT / "tests"), top_level_dir=str(ROOT))
            result = unittest.TestResult()
            suite.run(result)
        self.assertGreater(result.testsRun, 0)
        self.assertEqual(result.failures + result.errors, [])


if __name__ == "__main__":
    unittest.main()
