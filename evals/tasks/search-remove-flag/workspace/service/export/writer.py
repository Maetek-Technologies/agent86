"""Exports of order rows."""

from .. import features


def legacy_format(rows):
    """The old pipe-separated export, without a header."""
    return "\n".join("|".join(str(value) for value in row) for row in rows)


def new_format(rows, columns):
    """CSV with a header line."""
    lines = [",".join(columns)]
    lines += [",".join(str(value) for value in row) for row in rows]
    return "\n".join(lines)


def export(rows, columns=("id", "customer", "total")):
    if features.ENABLE_LEGACY_EXPORT:
        return legacy_format(rows)
    return new_format(rows, columns)


def file_extension():
    return ".txt" if features.ENABLE_LEGACY_EXPORT else ".csv"
