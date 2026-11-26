import unittest
from decimal import Decimal

from money import invoice_total, line_total, split_evenly


class MoneyTest(unittest.TestCase):
    def test_line(self):
        self.assertEqual(line_total("0.10", 3), Decimal("0.30"))
        self.assertEqual(line_total("1.005", 1), Decimal("1.01"))

    def test_invoice(self):
        self.assertEqual(invoice_total([("19.99", 1), ("5.01", 2)]), Decimal("30.01"))

    def test_split(self):
        self.assertEqual(split_evenly("10.00", 3), [Decimal("3.34"), Decimal("3.33"), Decimal("3.33")])


if __name__ == "__main__":
    unittest.main()
