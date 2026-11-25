"""Price quotes. The rules are in docs/pricing-spec.md."""

from decimal import ROUND_HALF_UP, Decimal

CENT = Decimal("0.01")


def cents(amount):
    return amount.quantize(CENT, rounding=ROUND_HALF_UP)


def price_quote(items, customer):
    if not items:
        raise ValueError("a quote needs at least one line")
    subtotal = Decimal(0)
    for item in items:
        if item["quantity"] < 1:
            raise ValueError("quantity must be at least 1")
        line = Decimal(item["unit_price"]) * item["quantity"]
        if item["quantity"] >= 10:
            line *= Decimal("0.9")
        subtotal += line
    if customer.get("loyalty"):
        subtotal *= Decimal("0.95")
    coupon = customer.get("coupon")
    if coupon == "SAVE5":
        subtotal -= Decimal(5)
    elif coupon == "SAVE10":
        if subtotal >= 40:
            subtotal -= Decimal(10)
    elif coupon is not None:
        raise ValueError(f"unknown coupon: {coupon}")
    goods = max(subtotal, Decimal(0))
    region = customer["region"]
    if region in ("AK", "HI"):
        shipping = Decimal("14.99")
    else:
        shipping = Decimal(0) if goods >= 50 else Decimal("4.99")
    tax = Decimal(0) if region == "OR" else goods * Decimal("0.08")
    return {"goods": cents(goods), "shipping": cents(shipping), "tax": cents(tax),
            "total": cents(goods + shipping + tax)}
