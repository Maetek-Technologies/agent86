import pathlib
import unittest

from csvline import parse_csv_line

ROOT = pathlib.Path(__file__).resolve().parent.parent


class Hidden(unittest.TestCase):
    def test_the_visible_cases(self):
        self.assertEqual(parse_csv_line("a,b,c"), ["a", "b", "c"])
        self.assertEqual(parse_csv_line(" a , b"), [" a ", " b"])
        self.assertEqual(parse_csv_line('x,"y,z"'), ["x", "y,z"])
        self.assertEqual(parse_csv_line('"say ""hi"""'), ['say "hi"'])
        self.assertEqual(parse_csv_line("a,,b"), ["a", "", "b"])
        self.assertEqual(parse_csv_line(""), [""])

    def test_edges(self):
        self.assertEqual(parse_csv_line("a,"), ["a", ""])
        self.assertEqual(parse_csv_line(","), ["", ""])
        self.assertEqual(parse_csv_line('""'), [""])
        self.assertEqual(parse_csv_line('"",x'), ["", "x"])
        self.assertEqual(parse_csv_line('a"b,c'), ['a"b', "c"])
        self.assertEqual(parse_csv_line('"a,b",,"c""d",e'), ["a,b", "", 'c"d', "e"])
        self.assertEqual(parse_csv_line('"x",'), ["x", ""])

    def test_errors(self):
        for bad in ['"open', 'a,"open', '"ab"c', '"a""', 'x,"y"z']:
            with self.assertRaises(ValueError, msg=bad):
                parse_csv_line(bad)

    def test_no_csv_module(self):
        source = (ROOT / "csvline.py").read_text(encoding="utf-8")
        self.assertNotIn("import csv", source)
        self.assertNotIn("from csv", source)


if __name__ == "__main__":
    unittest.main()
