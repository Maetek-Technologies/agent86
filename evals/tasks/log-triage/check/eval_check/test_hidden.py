import unittest

from billing.batch import run
from billing.invoice import build_invoice


class Hidden(unittest.TestCase):
    def test_a_customer_without_a_region(self):
        self.assertEqual(
            build_invoice({"id": 4471}, 100.0),
            {"customer": 4471, "region": "unknown", "amount": 100.0, "tax": 0.0, "total": 100.0},
        )

    def test_a_region_without_a_rate(self):
        invoice = build_invoice({"id": 9, "region": "jp"}, 50.0)
        self.assertEqual((invoice["region"], invoice["tax"], invoice["total"]), ("jp", 0.0, 50.0))

    def test_known_regions_are_unchanged(self):
        invoice = build_invoice({"id": 1, "region": "eu"}, 100.0)
        self.assertEqual((invoice["tax"], invoice["total"]), (20.0, 120.0))
        invoice = build_invoice({"id": 2, "region": "ca"}, 10.0)
        self.assertEqual((invoice["tax"], invoice["total"]), (0.5, 10.5))
        with self.assertRaises(ValueError):
            build_invoice({"id": 3, "region": "us"}, -1)

    def test_the_run_completes(self):
        customers = [{"id": 1, "region": "us"}, {"id": 4471}, {"id": 5, "region": "uk"}]
        invoices = run(customers, {1: 10.0, 4471: 20.0, 5: 30.0})
        self.assertEqual([invoice["total"] for invoice in invoices], [10.0, 20.0, 36.0])


if __name__ == "__main__":
    unittest.main()
