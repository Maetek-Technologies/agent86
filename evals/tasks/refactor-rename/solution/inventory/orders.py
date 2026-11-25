"""Orders."""

from .pricing import order_total


class Order:
    def __init__(self, customer, lines, tax_rate=0.0):
        self.customer = customer
        self.lines = list(lines)
        self.tax_rate = tax_rate

    def total(self, discount=0.0):
        return order_total(self.lines, tax_rate=self.tax_rate, discount=discount)

    def add(self, price, quantity):
        self.lines.append((price, quantity))
        return order_total(self.lines, tax_rate=self.tax_rate)
