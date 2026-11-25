"""Price quotes. The rules are in docs/pricing-spec.md."""

from decimal import Decimal


def price_quote(items, customer):
    """A quote for ``items`` (a list of dicts with "sku", "unit_price" as a
    string such as "12.50", and "quantity" as an int) for ``customer`` (a dict
    with "region", a two-letter code, and optionally "loyalty", a bool, and
    "coupon", a string).

    Returns a dict with "goods", "shipping", "tax", and "total", each a
    Decimal with two places, as docs/pricing-spec.md defines them.
    """
    raise NotImplementedError
