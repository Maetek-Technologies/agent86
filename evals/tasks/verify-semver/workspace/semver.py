"""Semantic Versioning 2.0.0 precedence."""


def compare_versions(a, b):
    """-1, 0, or 1 as version ``a`` has lower, equal, or higher precedence
    than ``b``, by the rules of Semantic Versioning 2.0.0:

    - A version is MAJOR.MINOR.PATCH, three non-negative integers with no
      leading zeros ("1.02.0" is invalid), optionally followed by "-" and a
      pre-release, then optionally "+" and build metadata.
    - Pre-release and build metadata are dot-separated identifiers of ASCII
      letters, digits, and hyphens, none empty. A numeric pre-release
      identifier has no leading zeros; build identifiers may.
    - Build metadata never affects precedence: "1.0.0+a" equals "1.0.0+b".
    - MAJOR, MINOR, and PATCH compare numerically, in that order.
    - With those equal, a version with a pre-release is lower than one
      without: "1.0.0-alpha" < "1.0.0".
    - Two pre-releases compare identifier by identifier, left to right:
      numeric identifiers numerically, others in ASCII order, and a numeric
      identifier is always lower than a non-numeric one. If every identifier
      so far is equal, the one with more identifiers is higher.

    Anything that is not a valid version raises ValueError.
    """
    raise NotImplementedError
