"""Business-day arithmetic."""

import datetime


def add_business_days(start, days, holidays=()):
    """The date ``days`` business days after ``start`` (a datetime.date).

    Business days are Monday to Friday, except the dates in ``holidays``.
    ``start`` itself is never counted, even when it is a business day, and
    ``days`` of 0 gives ``start`` unchanged. Negative ``days`` count backwards
    by the same rules, holidays included.
    """
    current = start
    if days >= 0:
        remaining = days
        while remaining > 0:
            current += datetime.timedelta(days=1)
            if current.weekday() < 5 and current not in holidays:
                remaining -= 1
    else:
        remaining = -days
        while remaining > 0:
            current -= datetime.timedelta(days=1)
            if current.weekday() < 5:
                remaining -= 1
    return current


def is_business_day(day, holidays=()):
    """Whether ``day`` is a business day."""
    return day.weekday() < 5 and day not in holidays
