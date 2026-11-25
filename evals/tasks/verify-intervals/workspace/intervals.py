"""Closed integer intervals."""


def merge_intervals(intervals):
    """Merge ``intervals``, an iterable of (start, end) pairs of ints with
    start <= end, each covering start to end inclusive.

    Returns a new list of (start, end) tuples, sorted, with every pair of
    intervals that overlap or touch merged into one. Touching includes ends
    that are adjacent integers: (1, 3) and (4, 6) cover 1 to 6 with no gap, so
    they merge into (1, 6); (1, 3) and (5, 6) do not.

    The input is never modified, and may be in any order. An empty input gives
    []. A pair with start > end, or a value that is not an int (bools
    included), raises ValueError.
    """
    raise NotImplementedError
