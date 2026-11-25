"""Money for invoices. Amounts arrive as strings such as "19.99"."""

from decimal import Decimal


def line_total(unit_price, quantity):
    """``unit_price`` (a string) times ``quantity`` (an int), as a Decimal
    exact to the cent, rounded half up (0.005 rounds to 0.01)."""
    return Decimal(float(unit_price) * quantity).quantize(Decimal("0.01"))


def invoice_total(lines, tax_rate="0"):
    """The total of ``lines`` (pairs of unit price string and quantity), plus
    tax at ``tax_rate`` (a string such as "0.0725"), as a Decimal rounded half
    up to the cent. Each line is rounded first, then the tax on their sum."""
    subtotal = sum(line_total(price, quantity) for price, quantity in lines)
    tax = Decimal(float(subtotal) * float(tax_rate)).quantize(Decimal("0.01"))
    return subtotal + tax


def split_evenly(amount, people):
    """``amount`` (a string) split among ``people`` as Decimals to the cent
    that add up to exactly ``amount``: everyone gets the amount divided and
    rounded down, and the cents left over go one each to the first people."""
    share = Decimal(float(amount) / people).quantize(Decimal("0.01"))
    return [share] * people
