import pathlib
import subprocess
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent

PLACES = {
    "normalize_space": "utils.strings",
    "title_case": "utils.strings",
    "initials": "utils.strings",
    "clamp": "utils.numbers",
    "percent": "utils.numbers",
    "chunks": "utils.iterables",
}


class Hidden(unittest.TestCase):
    def test_the_layout(self):
        self.assertFalse((ROOT / "utils.py").exists(), "utils.py is still there")
        for name in ("__init__.py", "strings.py", "numbers.py", "iterables.py"):
            self.assertTrue((ROOT / "utils" / name).is_file(), name)

    def test_each_function_lives_where_asked_and_is_reexported(self):
        import utils

        for name, module in PLACES.items():
            function = getattr(utils, name)
            self.assertEqual(function.__module__, module, name)

    def test_behaviour(self):
        from utils import chunks, clamp, initials, normalize_space, percent, title_case

        self.assertEqual(normalize_space("  a \t b\n"), "a b")
        self.assertEqual(title_case("the lord OF the rings"), "The Lord of the Rings")
        self.assertEqual(initials(" ada  lovelace "), "AL")
        self.assertEqual(clamp(150, 0, 99), 99)
        self.assertEqual(percent(1, 3), 33.3)
        self.assertEqual(percent(1, 0), 0.0)
        self.assertEqual(chunks(range(5), 2), [[0, 1], [2, 3], [4]])
        with self.assertRaises(ValueError):
            chunks([], 0)

    def test_the_callers(self):
        # A fresh interpreter, so nothing cached from this one hides a break.
        code = (
            "import app, report;"
            "print(app.greeting('ada lovelace', 120));"
            "print(report.summary(1, 4, ['c', 'a', 'b']))"
        )
        result = subprocess.run([sys.executable, "-c", code], cwd=ROOT, capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.splitlines(),
            ["Ada Lovelace (AL): 99 unread",
             "{'progress': 25.0, 'rows': ['a, b', 'c'], 'title': 'Weekly report'}"],
        )
        source = (ROOT / "app.py").read_text(encoding="utf-8")
        self.assertIn("from utils import clamp, initials, title_case", source)


if __name__ == "__main__":
    unittest.main()
