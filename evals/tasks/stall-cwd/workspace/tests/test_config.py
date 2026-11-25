import os
import tempfile
import unittest

from app.config_loader import load_settings


class ConfigTest(unittest.TestCase):
    def test_defaults_load_from_anywhere(self):
        before = os.getcwd()
        with tempfile.TemporaryDirectory() as elsewhere:
            os.chdir(elsewhere)
            try:
                settings = load_settings()
            finally:
                os.chdir(before)
        self.assertEqual(settings["database"], "sqlite:///data/app.db")
        self.assertEqual(settings["workers"], 4)


if __name__ == "__main__":
    unittest.main()
