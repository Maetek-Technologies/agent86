"""Assorted helpers."""

import re

_SPACE = re.compile(r"\s+")
SMALL_WORDS = {"a", "an", "and", "of", "the", "to", "in"}


def normalize_space(text):
    """``text`` with every run of whitespace as one space, trimmed."""
    return _SPACE.sub(" ", text).strip()


def title_case(text):
    """Title case, except small words after the first."""
    words = normalize_space(text).lower().split(" ")
    return " ".join(
        word if index and word in SMALL_WORDS else word.capitalize()
        for index, word in enumerate(words)
    )


def initials(name):
    """The upper-case initials of ``name``."""
    return "".join(part[0].upper() for part in normalize_space(name).split(" ") if part)


def clamp(value, low, high):
    """``value`` limited to [low, high]."""
    if low > high:
        raise ValueError("low must not exceed high")
    return max(low, min(high, value))


def percent(part, whole, digits=1):
    """``part`` as a percentage of ``whole``, rounded; 0.0 when ``whole`` is 0."""
    if whole == 0:
        return 0.0
    return round(100.0 * part / whole, digits)


def chunks(items, size):
    """Lists of ``size`` items from ``items``, the last possibly shorter."""
    if size < 1:
        raise ValueError("size must be at least 1")
    items = list(items)
    return [items[start:start + size] for start in range(0, len(items), size)]
