"""Print the most common words of a text file.

    python top_words.py FILE

Each line of output is a word, a tab, and how many times it appears. Words are
runs of letters, digits, and apostrophes. The most common come first; words
with the same count are in alphabetical order.
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


def main(argv=None):
    parser = argparse.ArgumentParser(description="Print the most common words of a text file.")
    parser.add_argument("file")
    args = parser.parse_args(argv)
    with open(args.file, encoding="utf-8") as handle:
        counts = count_words(handle.read())
    for word, count in top(counts, 10):
        print(f"{word}\t{count}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
