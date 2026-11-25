"""Write docs/pricing-spec.md, the same every time: about 30 KB, with the
eight rules spread through sections of background that do not matter."""

import pathlib
import random

RULES = [
    "**PQ-1 (line totals).** A line's total is its unit price times its quantity. "
    "A line whose quantity is 10 or more is a bulk line and gets 10% off its own total. "
    "A quantity below 1, or a quote with no lines at all, is an error: raise ValueError.",
    "**PQ-2 (subtotal).** The subtotal is the sum of the line totals, after any bulk discounts.",
    "**PQ-3 (loyalty).** A customer whose \"loyalty\" is true gets 5% off the subtotal. "
    "This comes after the bulk discounts of PQ-1 and before any coupon.",
    "**PQ-4 (coupons).** After the percentage discounts, a coupon takes a fixed amount off: "
    "\"SAVE5\" takes 5.00 off; \"SAVE10\" takes 10.00 off, but only when the amount it applies to "
    "is at least 40.00, and otherwise takes nothing off. Any other coupon code is an error: raise "
    "ValueError. A coupon never takes the amount below 0.00. What is left after the coupon is the "
    "quote's goods amount.",
    "**PQ-5 (shipping).** Shipping is 4.99, and free (0.00) when the goods amount is 50.00 or more. "
    "For the regions \"AK\" and \"HI\" shipping is 14.99 whatever the goods amount, and never free.",
    "**PQ-6 (tax).** Tax is 8% of the goods amount. Shipping is never taxed. Customers in the "
    "region \"OR\" pay no tax.",
    "**PQ-7 (rounding).** Nothing is rounded along the way: every step above works on exact "
    "decimal amounts. Only the four figures of the quote are rounded, each to cents, half up "
    "(0.005 rounds to 0.01): goods, shipping, tax, and total. The total is the exact goods plus "
    "shipping plus tax, rounded once.",
    "**PQ-8 (result).** The quote is a dict with exactly the keys \"goods\", \"shipping\", \"tax\", "
    "and \"total\", each a Decimal with exactly two decimal places.",
]

TOPICS = [
    "Background", "History of the quoting service", "Glossary", "Stakeholders",
    "Data sources", "Operational notes", "Audit trail", "Customer communications",
    "Regional considerations", "Performance targets", "Open questions", "Appendix",
]
WORDS = (
    "the quoting service team reviewed customer feedback and regional sales data while finance "
    "confirmed that reporting periods align with the warehouse schedule so that support staff can "
    "explain each invoice clearly during the onboarding process for new accounts"
).split()


def paragraph(rng):
    sentences = []
    for _ in range(rng.randint(4, 7)):
        sentences.append(" ".join(rng.choice(WORDS) for _ in range(rng.randint(12, 20))).capitalize() + ".")
    return " ".join(sentences)


def main():
    rng = random.Random(86)
    out = ["# Pricing specification for quotes", "",
           "This document is the reference for how quotes are priced. Its rules are numbered "
           "PQ-1 to PQ-8; everything else is background.", ""]
    rules = list(RULES)
    for index, topic in enumerate(TOPICS):
        out += [f"## {index + 1}. {topic}", ""]
        for _ in range(rng.randint(3, 4)):
            out += [paragraph(rng), ""]
        if index % 3 != 2 and rules:
            out += [rules.pop(0), ""]
            out += [paragraph(rng), ""]
    while rules:
        out += [rules.pop(0), ""]
    path = pathlib.Path("docs")
    path.mkdir(exist_ok=True)
    (path / "pricing-spec.md").write_text("\n".join(out), encoding="utf-8")


if __name__ == "__main__":
    main()
