"""String helpers."""

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
