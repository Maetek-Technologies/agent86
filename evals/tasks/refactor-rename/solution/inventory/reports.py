"""Reports over orders."""

from . import pricing


def monthly_report(orders):
    """Total revenue and the largest order's total, over ``orders``."""
    totals = [pricing.order_total(order.lines, tax_rate=order.tax_rate) for order in orders]
    return {
        "revenue": round(sum(totals), 2),
        "largest": max(totals, default=0.0),
        "count": len(totals),
    }
