"""The monthly orders report."""

from .. import features
from ..export import writer


def report_name(month):
    return f"orders-{month}{writer.file_extension()}"


def monthly_report(month, rows):
    body = writer.export(rows)
    if not features.ENABLE_LEGACY_EXPORT:
        body = f"# Orders for {month}\n{body}"
    if features.ENABLE_BULK_EDIT:
        body += "\n# bulk edit enabled"
    return report_name(month), body
