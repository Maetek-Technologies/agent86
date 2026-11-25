"""Money for invoices. Amounts arrive as strings such as "19.99"."""

from decimal import ROUND_DOWN, ROUND_HALF_UP, Decimal

CENT = Decimal("0.01")


def line_total(unit_price, quantity):
    return (Decimal(unit_price) * quantity).quantize(CENT, rounding=ROUND_HALF_UP)


def invoice_total(lines, tax_rate="0"):
    subtotal = sum((line_total(price, quantity) for price, quantity in lines), Decimal("0.00"))
    tax = (subtotal * Decimal(tax_rate)).quantize(CENT, rounding=ROUND_HALF_UP)
    return subtotal + tax


def split_evenly(amount, people):
    total = Decimal(amount).quantize(CENT)
    share = (total / people).quantize(CENT, rounding=ROUND_DOWN)
    left = int((total - share * people) / CENT)
    return [share + CENT if index < left else share for index in range(people)]
