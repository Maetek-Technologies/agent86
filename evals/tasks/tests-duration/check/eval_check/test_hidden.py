import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parent
MUTANTS = sorted((HERE / "mutants").glob("*.py"))


def run_suite_against(implementation):
    """Run the agent's tests/ with ``implementation`` as duration.py, in a
    fresh directory so nothing else of the workspace is involved."""
    with tempfile.TemporaryDirectory() as tmp:
        tmp = pathlib.Path(tmp)
        shutil.copytree(ROOT / "tests", tmp / "tests")
        shutil.copy(implementation, tmp / "duration.py")
        result = subprocess.run(
            [sys.executable, "-m", "unittest", "discover", "-s", "tests", "-t", "."],
            cwd=tmp, capture_output=True, text=True, timeout=120,
        )
    return result


class Hidden(unittest.TestCase):
    def test_the_suite_exists(self):
        self.assertTrue((ROOT / "tests" / "test_duration.py").is_file())

    def test_it_passes_on_the_real_implementation(self):
        result = run_suite_against(HERE / "reference_duration.py")
        self.assertEqual(result.returncode, 0, result.stderr[-3000:])
        self.assertNotIn("Ran 0 tests", result.stderr)

    def test_it_catches_every_mutant(self):
        self.assertEqual(len(MUTANTS), 6)
        survivors = [m.stem for m in MUTANTS if run_suite_against(m).returncode == 0]
        self.assertEqual(survivors, [], "mutants the suite does not catch")

    def test_duration_py_is_unchanged(self):
        self.assertEqual((ROOT / "duration.py").read_text(encoding="utf-8").replace("\r\n", "\n"),
                         (HERE / "reference_duration.py").read_text(encoding="utf-8").replace("\r\n", "\n"))


if __name__ == "__main__":
    unittest.main()
