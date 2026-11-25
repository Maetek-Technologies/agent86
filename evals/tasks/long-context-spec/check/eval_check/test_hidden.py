import random
import unittest
from decimal import ROUND_HALF_UP, Decimal

from quote import price_quote

CENT = Decimal("0.01")


def reference(items, customer):
    """The spec, PQ-1 to PQ-8."""
    if not items:
        raise ValueError
    subtotal = Decimal(0)
    for item in items:
        if item["quantity"] < 1:
            raise ValueError
        line = Decimal(item["unit_price"]) * item["quantity"]
        if item["quantity"] >= 10:
            line *= Decimal("0.9")
        subtotal += line
    if customer.get("loyalty"):
        subtotal *= Decimal("0.95")
    coupon = customer.get("coupon")
    if coupon == "SAVE5":
        subtotal -= 5
    elif coupon == "SAVE10":
        if subtotal >= 40:
            subtotal -= 10
    elif coupon is not None:
        raise ValueError
    goods = max(subtotal, Decimal(0))
    region = customer["region"]
    if region in ("AK", "HI"):
        shipping = Decimal("14.99")
    else:
        shipping = Decimal(0) if goods >= 50 else Decimal("4.99")
    tax = Decimal(0) if region == "OR" else goods * Decimal("0.08")
    cents = lambda amount: amount.quantize(CENT, rounding=ROUND_HALF_UP)  # noqa: E731
    return {"goods": cents(goods), "shipping": cents(shipping), "tax": cents(tax),
            "total": cents(goods + shipping + tax)}


def line(price, quantity, sku="x"):
    return {"sku": sku, "unit_price": price, "quantity": quantity}


class Hidden(unittest.TestCase):
    def check(self, items, customer):
        got = price_quote(items, customer)
        self.assertEqual(set(got), {"goods", "shipping", "tax", "total"})
        for key, value in got.items():
            self.assertIsInstance(value, Decimal, key)
            self.assertEqual(value.as_tuple().exponent, -2, f"{key} = {value}")
        self.assertEqual(got, reference(items, customer), (items, customer))

    def test_each_rule(self):
        self.check([line("3.00", 2)], {"region": "CA"})                      # small order, shipping, tax
        self.check([line("2.50", 10)], {"region": "CA"})                     # bulk
        self.check([line("2.50", 9), line("1.00", 12)], {"region": "NY"})    # bulk on one line only
        self.check([line("20.00", 3)], {"region": "CA", "loyalty": True})    # loyalty
        self.check([line("20.00", 3)], {"region": "CA", "coupon": "SAVE5"})  # coupon, free shipping
        self.check([line("20.00", 2)], {"region": "CA", "coupon": "SAVE10"}) # SAVE10 at 40.00
        self.check([line("19.99", 2)], {"region": "CA", "coupon": "SAVE10"}) # SAVE10 below 40: nothing
        self.check([line("3.00", 1)], {"region": "CA", "coupon": "SAVE5"})   # never below zero
        self.check([line("60.00", 1)], {"region": "AK"})                     # AK never free
        self.check([line("60.00", 1)], {"region": "HI", "loyalty": True})
        self.check([line("60.00", 1)], {"region": "OR"})                     # no tax in OR
        self.check([line("52.63", 1)], {"region": "CA", "loyalty": True})    # loyalty takes it under 50
        self.check([line("0.10", 1), line("0.20", 1)], {"region": "CA"})     # exact decimals
        self.check([line("1.05", 1)], {"region": "CA"})                      # tax 0.084 -> 0.08
        self.check([line("6.3125", 1)], {"region": "CA"})                    # half up at the end only

    def test_errors(self):
        for items, customer in [
            ([], {"region": "CA"}),
            ([line("1.00", 0)], {"region": "CA"}),
            ([line("1.00", 1)], {"region": "CA", "coupon": "FREE"}),
        ]:
            with self.assertRaises(ValueError):
                price_quote(items, customer)

    def test_many(self):
        rng = random.Random(86)
        for _ in range(300):
            items = [line(f"{rng.randint(1, 4000) / 100:.2f}", rng.choice([1, 2, 3, 9, 10, 11, 25]))
                     for _ in range(rng.randint(1, 4))]
            customer = {"region": rng.choice(["CA", "NY", "OR", "AK", "HI"])}
            if rng.random() < 0.5:
                customer["loyalty"] = rng.random() < 0.5
            if rng.random() < 0.5:
                customer["coupon"] = rng.choice(["SAVE5", "SAVE10"])
            self.check(items, customer)


if __name__ == "__main__":
    unittest.main()
