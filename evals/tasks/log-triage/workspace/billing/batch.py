"""The nightly billing run."""

import logging

from .invoice import build_invoice

log = logging.getLogger("billing.batch")


def run(customers, usage):
    """Build an invoice for every customer with usage; stop at the first failure."""
    invoices = []
    for customer in customers:
        amount = usage.get(customer["id"], 0)
        if amount == 0:
            log.info("skipped customer %s: no billable usage", customer["id"])
            continue
        try:
            invoice = build_invoice(customer, amount)
        except Exception:
            log.exception("invoice generation failed for customer %s", customer["id"])
            log.error("aborting run after 1 failure")
            raise
        invoices.append(invoice)
    return invoices
