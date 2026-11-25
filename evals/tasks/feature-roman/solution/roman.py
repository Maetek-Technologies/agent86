"""Roman numerals."""

PAIRS = [(1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"), (90, "XC"),
         (50, "L"), (40, "XL"), (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I")]
VALUES = {"I": 1, "V": 5, "X": 10, "L": 50, "C": 100, "D": 500, "M": 1000}


def to_roman(number):
    if type(number) is not int or not 1 <= number <= 3999:
        raise ValueError(f"not a number from 1 to 3999: {number!r}")
    parts = []
    for value, letters in PAIRS:
        while number >= value:
            parts.append(letters)
            number -= value
    return "".join(parts)


def from_roman(text):
    if not isinstance(text, str) or not text or any(c not in VALUES for c in text):
        raise ValueError(f"not a Roman numeral: {text!r}")
    total = 0
    for index, letter in enumerate(text):
        value = VALUES[letter]
        if index + 1 < len(text) and VALUES[text[index + 1]] > value:
            total -= value
        else:
            total += value
    if not 1 <= total <= 3999 or to_roman(total) != text:
        raise ValueError(f"not a Roman numeral in standard form: {text!r}")
    return total
