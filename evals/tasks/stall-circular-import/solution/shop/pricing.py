"""Prices."""

from shop import models


def line_price(product, quantity):
    if not isinstance(product, models.Product):
        raise TypeError("line_price needs a Product")
    return product.price * quantity


def price_with_tax(product, rate):
    if not isinstance(product, models.Product):
        raise TypeError("price_with_tax needs a Product")
    return product.price * (1 + rate) if product.taxable else product.price
