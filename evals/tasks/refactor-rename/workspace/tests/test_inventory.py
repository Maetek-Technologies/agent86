import unittest

from inventory import Order, monthly_report
from inventory.pricing import calc_total


class InventoryTest(unittest.TestCase):
    def test_total(self):
        self.assertEqual(calc_total([(10.0, 2), (5.0, 1)], tax_rate=0.1), 27.5)

    def test_order(self):
        order = Order("ana", [(10.0, 1)], tax_rate=0.2)
        self.assertEqual(order.add(5.0, 2), 24.0)
        self.assertEqual(order.total(discount=0.5), 12.0)

    def test_report(self):
        report = monthly_report([Order("a", [(1.0, 3)]), Order("b", [(4.0, 1)])])
        self.assertEqual(report, {"revenue": 7.0, "largest": 4.0, "count": 2})


if __name__ == "__main__":
    unittest.main()
