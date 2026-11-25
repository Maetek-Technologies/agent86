import unittest

from roman import from_roman, to_roman


class Hidden(unittest.TestCase):
    def test_known(self):
        for number, text in [(1, "I"), (4, "IV"), (9, "IX"), (14, "XIV"), (40, "XL"),
                             (90, "XC"), (400, "CD"), (944, "CMXLIV"), (1994, "MCMXCIV"),
                             (2026, "MMXXVI"), (3999, "MMMCMXCIX")]:
            self.assertEqual(to_roman(number), text)
            self.assertEqual(from_roman(text), number)

    def test_round_trip(self):
        for number in range(1, 4000):
            self.assertEqual(from_roman(to_roman(number)), number)

    def test_bad_numbers(self):
        for bad in (0, -1, 4000, 10_000, True, 1.0, "5", None):
            with self.assertRaises(ValueError, msg=repr(bad)):
                to_roman(bad)

    def test_bad_numerals(self):
        for bad in ("", "IIII", "VV", "IC", "XM", "MMMM", "mcm", "IIV", "VX", "LL",
                    "DM", "XIIII", "ABC", " X", "CMM", "IXI"):
            with self.assertRaises(ValueError, msg=repr(bad)):
                from_roman(bad)


if __name__ == "__main__":
    unittest.main()
