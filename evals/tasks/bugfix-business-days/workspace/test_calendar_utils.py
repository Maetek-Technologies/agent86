import datetime
import unittest

from calendar_utils import add_business_days

D = datetime.date


class AddBusinessDaysTest(unittest.TestCase):
    def test_over_a_weekend(self):
        # Friday 2026-11-20 plus one business day is Monday 2026-11-23.
        self.assertEqual(add_business_days(D(2026, 11, 20), 1), D(2026, 11, 23))

    def test_back_over_a_holiday(self):
        # Thanksgiving, Thursday 2026-11-26, is not a business day.
        holidays = {D(2026, 11, 26)}
        self.assertEqual(add_business_days(D(2026, 11, 27), -1, holidays), D(2026, 11, 25))


if __name__ == "__main__":
    unittest.main()
