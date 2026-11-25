import unittest

from duration import parse_duration


class ParseDurationTest(unittest.TestCase):
    def test_values(self):
        cases = {"1h30m": 5400, "90m": 5400, "45s": 45, "2d": 172_800, "1d2h3m4s": 93_784,
                 "0s": 0, "0h0m": 0, " 5m ": 300, "1h": 3600}
        for text, seconds in cases.items():
            self.assertEqual(parse_duration(text), seconds, text)

    def test_case_insensitive(self):
        self.assertEqual(parse_duration("1H30M"), 5400)
        self.assertEqual(parse_duration("2D"), 172_800)

    def test_errors(self):
        for bad in ("", "   ", "90", "1h30", "h", "5w", "1h1h", "30m1h", "1s1m", "-5m", "1.5h", "1h 30m", "abc"):
            with self.assertRaises(ValueError, msg=repr(bad)):
                parse_duration(bad)


if __name__ == "__main__":
    unittest.main()
