"""Roman numerals."""


def to_roman(number):
    """``number`` (an int from 1 to 3999) as a Roman numeral in standard form,
    upper case, with subtractive pairs (IV, IX, XL, XC, CD, CM): 1994 is
    "MCMXCIV". Anything else (0, negatives, 4000 and up, a bool, a float)
    raises ValueError.
    """
    raise NotImplementedError


def from_roman(text):
    """The int a Roman numeral in standard form stands for: ``from_roman(s)``
    is ``n`` exactly when ``to_roman(n) == s``. Upper case only. Anything else,
    such as "IIII", "VV", "IC", "MMMM", "", or "mcm", raises ValueError.
    """
    raise NotImplementedError
