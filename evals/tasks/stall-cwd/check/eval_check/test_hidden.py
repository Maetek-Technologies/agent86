import os
import pathlib
import tempfile
import unittest

from app.config_loader import load_settings

ROOT = pathlib.Path(__file__).resolve().parent.parent
ORIGINAL = "[database]\nurl = sqlite:///data/app.db\n\n[app]\nworkers = 4\nname = ledger\n"


class Hidden(unittest.TestCase):
    def test_defaults_from_another_directory(self):
        before = os.getcwd()
        with tempfile.TemporaryDirectory() as elsewhere:
            os.chdir(elsewhere)
            try:
                settings = load_settings()
            finally:
                os.chdir(before)
        self.assertEqual(
            settings, {"database": "sqlite:///data/app.db", "workers": 4, "name": "ledger"}
        )

    def test_defaults_from_the_package_directory_and_the_root(self):
        before = os.getcwd()
        for where in (ROOT, ROOT / "app"):
            os.chdir(where)
            try:
                self.assertEqual(load_settings()["workers"], 4)
            finally:
                os.chdir(before)

    def test_a_given_path_is_used(self):
        with tempfile.TemporaryDirectory() as elsewhere:
            other = pathlib.Path(elsewhere) / "other.ini"
            other.write_text("[database]\nurl = x\n[app]\nworkers = 9\nname = y\n", encoding="utf-8")
            self.assertEqual(load_settings(str(other))["workers"], 9)

    def test_the_settings_file_is_untouched(self):
        text = (ROOT / "app" / "settings.ini").read_text(encoding="utf-8").replace("\r\n", "\n")
        self.assertEqual(text, ORIGINAL)
        self.assertFalse((ROOT / "settings.ini").exists(), "settings.ini was copied to the root")


if __name__ == "__main__":
    unittest.main()
