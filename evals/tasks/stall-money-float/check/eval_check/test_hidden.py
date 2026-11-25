import unittest
from decimal import Decimal

from money import invoice_total, line_total, split_evenly

D = Decimal


class Hidden(unittest.TestCase):
    def test_lines(self):
        self.assertEqual(line_total("0.10", 3), D("0.30"))
        self.assertEqual(line_total("2.675", 1), D("2.68"))   # float rounding gives 2.67
        self.assertEqual(line_total("1.005", 1), D("1.01"))   # float rounding gives 1.00
        self.assertEqual(line_total("0.125", 3), D("0.38"))
        self.assertEqual(line_total("19.99", 0), D("0.00"))
        for value in (line_total("1.1", 3), line_total("5", 2)):
            self.assertEqual(value.as_tuple().exponent, -2)

    def test_invoices(self):
        self.assertEqual(invoice_total([("19.99", 1), ("5.01", 2)]), D("30.01"))
        self.assertEqual(invoice_total([("2.675", 1)], "0"), D("2.68"))
        # 10.05 at 5%: tax 0.5025 rounds to 0.50.
        self.assertEqual(invoice_total([("10.05", 1)], "0.05"), D("10.55"))
        # 1.15 at 10%: tax 0.115 rounds half up to 0.12 (a float gives 0.11).
        self.assertEqual(invoice_total([("1.15", 1)], "0.1"), D("1.27"))
        self.assertEqual(invoice_total([], "0.2"), D("0.00"))

    def test_splits(self):
        self.assertEqual(split_evenly("10.00", 3), [D("3.34"), D("3.33"), D("3.33")])
        self.assertEqual(split_evenly("0.05", 3), [D("0.02"), D("0.02"), D("0.01")])
        self.assertEqual(split_evenly("100", 4), [D("25.00")] * 4)
        self.assertEqual(split_evenly("0.01", 2), [D("0.01"), D("0.00")])
        for amount, people in (("123.45", 7), ("0.99", 4), ("1000.01", 3)):
            parts = split_evenly(amount, people)
            self.assertEqual(sum(parts), D(amount))
            self.assertEqual(len(parts), people)
            self.assertLessEqual(max(parts) - min(parts), D("0.01"))


if __name__ == "__main__":
    unittest.main()
