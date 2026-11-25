"""A tiny report."""

import utils


def summary(done, total, names):
    rows = [", ".join(group) for group in utils.chunks(sorted(names), 2)]
    return {"progress": utils.percent(done, total), "rows": rows, "title": utils.normalize_space("  Weekly   report ")}
