"""Text helpers."""

import re
import unicodedata


def slugify(text, max_length=None):
    """Turn ``text`` into a URL slug (see the task's docstring)."""
    ascii_text = unicodedata.normalize("NFKD", text).encode("ascii", "ignore").decode("ascii")
    slug = re.sub(r"[^a-z0-9]+", "-", ascii_text.lower()).strip("-")
    if max_length is None or len(slug) <= max_length:
        return slug
    cut = slug[:max_length]
    if slug[max_length] == "-":
        return cut.rstrip("-")
    if "-" in cut:
        return cut[: cut.rindex("-")]
    return cut
