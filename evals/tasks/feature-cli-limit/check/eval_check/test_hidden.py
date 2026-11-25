import pathlib
import subprocess
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
TEXT = "B a b A c a b a\nC d D d' x y z w v u t s r q\n"


def run(*args):
    with tempfile.TemporaryDirectory() as tmp:
        path = pathlib.Path(tmp) / "in.txt"
        path.write_text(TEXT, encoding="utf-8")
        result = subprocess.run(
            [sys.executable, str(ROOT / "top_words.py"), str(path), *args],
            capture_output=True, text=True, timeout=60,
        )
    return result.returncode, result.stdout.splitlines()


class Hidden(unittest.TestCase):
    def test_default_is_unchanged(self):
        code, lines = run()
        self.assertEqual(code, 0)
        self.assertEqual(lines, ["a\t3", "b\t2", "A\t1", "B\t1", "C\t1", "D\t1", "c\t1",
                                 "d\t1", "d'\t1", "q\t1"])

    def test_limit(self):
        self.assertEqual(run("--limit", "2")[1], ["a\t3", "b\t2"])
        self.assertEqual(run("--limit", "1")[1], ["a\t3"])
        self.assertEqual(len(run("--limit", "100")[1]), 19)

    def test_bad_limits(self):
        for bad in ("0", "-3", "two", "1.5"):
            code, lines = run("--limit", bad)
            self.assertEqual(code, 2, bad)
            self.assertEqual(lines, [], bad)

    def test_ignore_case(self):
        code, lines = run("--ignore-case")
        self.assertEqual(code, 0)
        self.assertEqual(lines[:4], ["a\t4", "b\t3", "c\t2", "d\t2"])
        self.assertTrue(all(line == line.lower() for line in lines))

    def test_both(self):
        self.assertEqual(run("--ignore-case", "--limit", "3")[1], ["a\t4", "b\t3", "c\t2"])
        self.assertEqual(run("--limit", "3", "--ignore-case")[1], ["a\t4", "b\t3", "c\t2"])


if __name__ == "__main__":
    unittest.main()
