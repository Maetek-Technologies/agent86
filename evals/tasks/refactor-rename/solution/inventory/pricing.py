"""Prices and totals."""

import warnings


def order_total(lines, tax_rate=0.0, discount=0.0):
    """The total of ``lines`` (pairs of unit price and quantity), less
    ``discount`` (a fraction), plus ``tax_rate`` (a fraction), rounded to cents.
    """
    subtotal = sum(price * quantity for price, quantity in lines)
    discounted = subtotal * (1 - discount)
    return round(discounted * (1 + tax_rate), 2)


def calc_total(*args, **kwargs):
    """Deprecated: use ``order_total``."""
    warnings.warn("calc_total is deprecated; use order_total", DeprecationWarning, stacklevel=2)
    return order_total(*args, **kwargs)


def unit_price(price, quantity):
    """The price of one item when ``quantity`` items cost ``price``."""
    if quantity <= 0:
        raise ValueError("quantity must be positive")
    return round(price / quantity, 2)
