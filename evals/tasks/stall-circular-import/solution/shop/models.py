"""Products and baskets."""

from dataclasses import dataclass, field

from shop import pricing


@dataclass
class Product:
    sku: str
    price: float
    taxable: bool = True


@dataclass
class Basket:
    lines: list = field(default_factory=list)

    def add(self, product, quantity=1):
        self.lines.append((product, quantity))

    def total(self):
        return round(sum(pricing.line_price(product, quantity) for product, quantity in self.lines), 2)

    def total_with_tax(self, rate):
        return round(sum(pricing.price_with_tax(product, rate) * quantity for product, quantity in self.lines), 2)
