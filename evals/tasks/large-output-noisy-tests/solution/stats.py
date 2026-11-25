"""Small statistics helpers for the sensor pipeline."""

import logging

log = logging.getLogger("stats")


def mean(values):
    values = list(values)
    if not values:
        raise ValueError("mean of no values")
    return sum(values) / len(values)


def median(values):
    """The middle value of ``values`` once sorted; for an even count, the mean
    of the two middle values. No values raises ValueError."""
    ordered = sorted(values)
    if not ordered:
        raise ValueError("median of no values")
    middle = len(ordered) // 2
    if len(ordered) % 2 == 0:
        return (ordered[middle - 1] + ordered[middle]) / 2
    return ordered[middle]


def smooth(readings, window=3):
    """Each reading replaced by the median of the ``window`` readings ending at it."""
    smoothed = []
    for index, reading in enumerate(readings):
        start = max(0, index - window + 1)
        value = median(readings[start:index + 1])
        log.info("smooth: reading %d raw=%r window=%r -> %r", index, reading, readings[start:index + 1], value)
        smoothed.append(value)
    return smoothed
