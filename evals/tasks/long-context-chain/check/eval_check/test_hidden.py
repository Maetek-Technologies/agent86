import os
import pathlib
import re
import unittest

CODES = [
    "AMBERFOX", "GLASSWREN", "TINMOTH", "BRASSOWL", "SILKHARE", "COPPERELK",
    "SLATEJAY", "IVORYCRAB", "FLINTMOLE", "OPALNEWT", "RUSTLYNX", "PEARLMINK",
]
TOTAL = 281


def final_answer():
    return pathlib.Path(os.environ["EVAL_FINAL_FILE"]).read_text(encoding="utf-8")


class Hidden(unittest.TestCase):
    def test_every_code_in_order(self):
        answer = final_answer().upper()
        found = [code for code in re.findall(r"[A-Z]{5,}", answer) if code in CODES]
        # The first mention of each, in order of first mention.
        seen = []
        for code in found:
            if code not in seen:
                seen.append(code)
        self.assertEqual(seen, CODES)

    def test_the_sum(self):
        numbers = [int(n) for n in re.findall(r"\b\d+\b", final_answer().replace(",", ""))]
        self.assertIn(TOTAL, numbers)


if __name__ == "__main__":
    unittest.main()
