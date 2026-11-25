"""The command line."""

import argparse

from . import features
from .reports.monthly import monthly_report


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("month")
    args = parser.parse_args(argv)
    name, body = monthly_report(args.month, [(1, "ana", 9.5), (2, "bo", 12.0)])
    if features.ENABLE_LEGACY_EXPORT:
        print("warning: using the legacy export format")
    print(name)
    print(body)
    return 0
