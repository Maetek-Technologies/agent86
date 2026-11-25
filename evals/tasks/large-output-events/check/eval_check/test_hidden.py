import collections
import datetime
import json
import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent


def november_purchases():
    counts = collections.Counter()
    with (ROOT / "data" / "events.jsonl").open(encoding="utf-8") as handle:
        for line in handle:
            event = json.loads(line)
            if event["type"] != "purchase":
                continue
            moment = datetime.datetime.fromisoformat(event["ts"]).astimezone(datetime.timezone.utc)
            if (moment.year, moment.month) == (2026, 11):
                counts[event["user"]] += 1
    return counts


class Hidden(unittest.TestCase):
    def test_the_answer(self):
        counts = november_purchases()
        (winner, top), (_, second) = counts.most_common(2)
        self.assertGreater(top, second, "the data no longer has a single winner")
        path = ROOT / "answer.txt"
        self.assertTrue(path.is_file(), "answer.txt was not written")
        self.assertEqual(path.read_text(encoding="utf-8").strip().strip("\"'`"), winner)


if __name__ == "__main__":
    unittest.main()
