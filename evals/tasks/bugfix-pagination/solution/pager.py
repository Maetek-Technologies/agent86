"""Split a list into pages."""


def paginate(items, page, per_page=10):
    """Return the items on ``page`` (counted from 1), ``per_page`` to a page.

    A page past the end is an empty list. ``page`` below 1, or ``per_page``
    below 1, raises ValueError.
    """
    if page < 1 or per_page < 1:
        raise ValueError("page and per_page must be at least 1")
    start = (page - 1) * per_page
    return items[start:start + per_page]


def page_count(total, per_page=10):
    """How many pages ``total`` items fill. A partly filled last page counts
    as a page; 0 items fill 0 pages. ``per_page`` below 1 raises ValueError.
    """
    if per_page < 1:
        raise ValueError("per_page must be at least 1")
    return -(-total // per_page)
