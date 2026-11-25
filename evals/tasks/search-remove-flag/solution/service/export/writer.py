"""Exports of order rows."""


def new_format(rows, columns):
    """CSV with a header line."""
    lines = [",".join(columns)]
    lines += [",".join(str(value) for value in row) for row in rows]
    return "\n".join(lines)


def export(rows, columns=("id", "customer", "total")):
    return new_format(rows, columns)


def file_extension():
    return ".csv"
