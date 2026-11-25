"""The monthly orders report."""

from .. import features
from ..export import writer


def report_name(month):
    return f"orders-{month}{writer.file_extension()}"


def monthly_report(month, rows):
    body = f"# Orders for {month}\n{writer.export(rows)}"
    if features.ENABLE_BULK_EDIT:
        body += "\n# bulk edit enabled"
    return report_name(month), body
