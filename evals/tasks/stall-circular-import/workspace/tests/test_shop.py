import unittest

from shop.models import Basket, Product
from shop.pricing import line_price


class ShopTest(unittest.TestCase):
    def test_basket(self):
        basket = Basket()
        basket.add(Product("a", 2.5), 2)
        basket.add(Product("b", 10.0, taxable=False))
        self.assertEqual(basket.total(), 15.0)
        self.assertEqual(basket.total_with_tax(0.2), 16.0)

    def test_type_check(self):
        with self.assertRaises(TypeError):
            line_price("not a product", 1)


if __name__ == "__main__":
    unittest.main()
