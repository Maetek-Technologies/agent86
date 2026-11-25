"""Write logs/nightly.log, the same every time: about 400 KB of ordinary
lines with one failure about 70% of the way in, past the first 100,000
characters a single read_file returns."""

import pathlib
import random

MODULES = ["billing.batch", "billing.ledger", "billing.mailer", "billing.export", "core.db"]
MESSAGES = [
    "processed invoice {n} for customer {c} in {ms} ms",
    "ledger entry {n} reconciled",
    "queued receipt mail for customer {c}",
    "exported batch {n} ({ms} rows)",
    "connection pool at {ms}% capacity",
    "skipped customer {c}: no billable usage",
]
FAILURE = """2026-11-20 02:{minute:02d}:07 ERROR billing.batch: invoice generation failed for customer 4471
Traceback (most recent call last):
  File "billing/batch.py", line 19, in run
    invoice = build_invoice(customer, amount)
  File "billing/invoice.py", line 16, in build_invoice
    region = customer["region"]
KeyError: 'region'
2026-11-20 02:{minute:02d}:07 ERROR billing.batch: aborting run after 1 failure
"""


def main():
    rng = random.Random(86)
    lines = []
    size = 0
    target = 400_000
    failed = False
    while size < target:
        minute = min(59, size * 60 // target)
        if not failed and size > target * 0.7:
            entry = FAILURE.format(minute=minute)
            failed = True
        else:
            level = rng.choice(["INFO"] * 6 + ["DEBUG"] * 3 + ["WARNING"])
            message = rng.choice(MESSAGES).format(
                n=rng.randint(10_000, 99_999), c=rng.randint(1_000, 9_999), ms=rng.randint(1, 900)
            )
            entry = f"2026-11-20 02:{minute:02d}:{rng.randint(0, 59):02d} {level} {rng.choice(MODULES)}: {message}\n"
        lines.append(entry)
        size += len(entry)
    path = pathlib.Path("logs")
    path.mkdir(exist_ok=True)
    (path / "nightly.log").write_text("".join(lines), encoding="utf-8")


if __name__ == "__main__":
    main()
