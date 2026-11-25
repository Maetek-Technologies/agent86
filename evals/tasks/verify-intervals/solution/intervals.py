"""Closed integer intervals."""


def merge_intervals(intervals):
    pairs = []
    for start, end in intervals:
        if type(start) is not int or type(end) is not int:
            raise ValueError(f"not a pair of ints: {(start, end)!r}")
        if start > end:
            raise ValueError(f"start after end: {(start, end)!r}")
        pairs.append((start, end))
    merged = []
    for start, end in sorted(pairs):
        if merged and start <= merged[-1][1] + 1:
            merged[-1] = (merged[-1][0], max(merged[-1][1], end))
        else:
            merged.append((start, end))
    return merged
