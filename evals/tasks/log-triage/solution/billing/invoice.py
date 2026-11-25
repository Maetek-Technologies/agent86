"""Invoices."""

TAX_BY_REGION = {"eu": 0.2, "uk": 0.2, "us": 0.0, "ca": 0.05}


def build_invoice(customer, amount):
    """The invoice for ``amount`` owed by ``customer`` (a dict with an "id").

    Customers who signed up before 2024 have no "region" on file; they are
    billed as region "unknown", with no tax. A region we have no rate for is
    also billed with no tax.
    """
    if amount < 0:
        raise ValueError("amount must not be negative")
    customer_id = customer["id"]
    region = customer.get("region", "unknown")
    tax = round(amount * TAX_BY_REGION.get(region, 0.0), 2)
    return {
        "customer": customer_id,
        "region": region,
        "amount": amount,
        "tax": tax,
        "total": round(amount + tax, 2),
    }
