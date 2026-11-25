"""Durations written like "1h30m"."""

import re

UNITS = {"d": 86_400, "h": 3_600, "m": 60, "s": 1}
PART = re.compile(r"(\d+)([dhms])")


def parse_duration(text):
    """The number of seconds ``text`` stands for, as an int.

    ``text`` is one or more parts, each a whole number followed by a unit: "d"
    (days), "h" (hours), "m" (minutes), or "s" (seconds), with nothing between
    the parts: "1h30m" is 5400. Units are case-insensitive ("1H30M" is 5400),
    each may appear at most once, and they must come in that order: days,
    hours, minutes, seconds. A number may be larger than the next unit up
    ("90m" is 5400) and may be 0. Spaces around the whole text are ignored.

    Anything else raises ValueError: an empty text, a number without a unit
    ("90"), a unit without a number ("h"), an unknown unit ("5w"), a repeated
    unit ("1h1h"), units out of order ("30m1h"), a sign or a decimal point
    ("-5m", "1.5h"), or spaces between parts ("1h 30m").
    """
    text = text.strip()
    if not text:
        raise ValueError("empty duration")
    total = 0
    position = 0
    last = -1
    order = list(UNITS)
    while position < len(text):
        match = PART.match(text, position)
        if match is None:
            raise ValueError(f"bad duration: {text!r}")
        number, unit = match.groups()
        rank = order.index(unit)
        if rank <= last:
            raise ValueError(f"unit {unit!r} repeated or out of order in {text!r}")
        last = rank
        total += int(number) * UNITS[unit]
        position = match.end()
    return total
