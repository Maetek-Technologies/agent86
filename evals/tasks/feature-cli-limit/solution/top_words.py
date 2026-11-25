"""Print the most common words of a text file.

    python top_words.py FILE [--limit N] [--ignore-case]
"""

import argparse
import collections
import re
import sys

WORD = re.compile(r"[A-Za-z0-9']+")


def count_words(text):
    return collections.Counter(WORD.findall(text))


def top(counts, limit):
    return sorted(counts.items(), key=lambda item: (-item[1], item[0]))[:limit]


def positive(text):
    try:
        value = int(text)
    except ValueError:
        raise argparse.ArgumentTypeError(f"not a whole number: {text!r}")
    if value < 1:
        raise argparse.ArgumentTypeError("must be at least 1")
    return value


def main(argv=None):
    parser = argparse.ArgumentParser(description="Print the most common words of a text file.")
    parser.add_argument("file")
    parser.add_argument("--limit", type=positive, default=10)
    parser.add_argument("--ignore-case", action="store_true")
    args = parser.parse_args(argv)
    with open(args.file, encoding="utf-8") as handle:
        text = handle.read()
    if args.ignore_case:
        text = text.lower()
    for word, count in top(count_words(text), args.limit):
        print(f"{word}\t{count}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
