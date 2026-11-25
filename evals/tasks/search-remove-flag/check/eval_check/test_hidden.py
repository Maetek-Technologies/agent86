import contextlib
import io
import json
import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
GONE = ("ENABLE_LEGACY_EXPORT", "enable_legacy_export", "legacy_format", "legacy export")


class Hidden(unittest.TestCase):
    def test_no_trace_of_the_flag(self):
        for path in ROOT.rglob("*"):
            if path.suffix not in (".py", ".json") or "eval_check" in path.parts:
                continue
            text = path.read_text(encoding="utf-8")
            for word in GONE:
                self.assertNotIn(word, text, f"{word} in {path.relative_to(ROOT)}")

    def test_the_other_flags_stay(self):
        from service import features

        self.assertIs(features.ENABLE_DARK_MODE, False)
        self.assertIs(features.ENABLE_BULK_EDIT, True)
        flags = json.loads((ROOT / "config" / "flags.json").read_text(encoding="utf-8"))
        self.assertEqual(flags, {"enable_dark_mode": False, "enable_bulk_edit": True})

    def test_behaviour_is_the_flag_off(self):
        from service.export import writer
        from service.reports.monthly import monthly_report

        rows = [(1, "ana", 9.5), (2, "bo", 12.0)]
        self.assertEqual(writer.export(rows), "id,customer,total\n1,ana,9.5\n2,bo,12.0")
        self.assertEqual(writer.export(rows, ("a", "b", "c")), "a,b,c\n1,ana,9.5\n2,bo,12.0")
        self.assertEqual(writer.file_extension(), ".csv")
        self.assertEqual(
            monthly_report("2026-11", rows[:1]),
            ("orders-2026-11.csv", "# Orders for 2026-11\nid,customer,total\n1,ana,9.5\n# bulk edit enabled"),
        )

    def test_the_command_line(self):
        from service import cli

        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(cli.main(["2026-11"]), 0)
        self.assertEqual(
            out.getvalue().splitlines(),
            ["orders-2026-11.csv", "# Orders for 2026-11", "id,customer,total", "1,ana,9.5",
             "2,bo,12.0", "# bulk edit enabled"],
        )


if __name__ == "__main__":
    unittest.main()
