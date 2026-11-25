import unittest

from csvline import parse_csv_line


class ParseTest(unittest.TestCase):
    def test_plain(self):
        self.assertEqual(parse_csv_line("a,b,c"), ["a", "b", "c"])

    def test_spaces_are_kept(self):
        self.assertEqual(parse_csv_line(" a , b"), [" a ", " b"])

    def test_quoted_comma(self):
        self.assertEqual(parse_csv_line('x,"y,z"'), ["x", "y,z"])

    def test_doubled_quote(self):
        self.assertEqual(parse_csv_line('"say ""hi"""'), ['say "hi"'])

    def test_empty_fields(self):
        self.assertEqual(parse_csv_line("a,,b"), ["a", "", "b"])
        self.assertEqual(parse_csv_line(""), [""])

    def test_unterminated(self):
        with self.assertRaises(ValueError):
            parse_csv_line('"open')


if __name__ == "__main__":
    unittest.main()
