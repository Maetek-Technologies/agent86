"""Iterable helpers."""


def chunks(items, size):
    """Lists of ``size`` items from ``items``, the last possibly shorter."""
    if size < 1:
        raise ValueError("size must be at least 1")
    items = list(items)
    return [items[start:start + size] for start in range(0, len(items), size)]
