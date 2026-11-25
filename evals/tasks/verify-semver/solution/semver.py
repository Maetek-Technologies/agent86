"""Semantic Versioning 2.0.0 precedence."""

import re

CORE = r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
PRE_ID = r"(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
BUILD_ID = r"[0-9A-Za-z-]+"
VERSION = re.compile(
    rf"^{CORE}(?:-({PRE_ID}(?:\.{PRE_ID})*))?(?:\+({BUILD_ID}(?:\.{BUILD_ID})*))?$"
)


def parse(version):
    match = VERSION.match(version) if isinstance(version, str) else None
    if match is None:
        raise ValueError(f"not a semantic version: {version!r}")
    major, minor, patch, pre, _build = match.groups()
    return (int(major), int(minor), int(patch)), (pre.split(".") if pre else [])


def compare_identifiers(a, b):
    a_numeric, b_numeric = a.isdigit(), b.isdigit()
    if a_numeric and b_numeric:
        return (int(a) > int(b)) - (int(a) < int(b))
    if a_numeric != b_numeric:
        return -1 if a_numeric else 1
    return (a > b) - (a < b)


def compare_versions(a, b):
    (core_a, pre_a), (core_b, pre_b) = parse(a), parse(b)
    if core_a != core_b:
        return (core_a > core_b) - (core_a < core_b)
    if not pre_a or not pre_b:
        return (not pre_a) - (not pre_b)
    for x, y in zip(pre_a, pre_b):
        result = compare_identifiers(x, y)
        if result:
            return result
    return (len(pre_a) > len(pre_b)) - (len(pre_a) < len(pre_b))
