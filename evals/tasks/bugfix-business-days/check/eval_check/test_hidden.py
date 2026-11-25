import datetime
import unittest

from calendar_utils import add_business_days

D = datetime.date
THANKSGIVING = D(2026, 11, 26)
CHRISTMAS = D(2026, 12, 25)


class Hidden(unittest.TestCase):
    def test_forwards(self):
        self.assertEqual(add_business_days(D(2026, 11, 20), 1), D(2026, 11, 23))
        self.assertEqual(add_business_days(D(2026, 11, 25), 1, {THANKSGIVING}), D(2026, 11, 27))
        self.assertEqual(add_business_days(D(2026, 11, 21), 1), D(2026, 11, 23))
        self.assertEqual(add_business_days(D(2026, 12, 24), 2, {CHRISTMAS}), D(2026, 12, 29))

    def test_backwards(self):
        self.assertEqual(add_business_days(D(2026, 11, 27), -1, {THANKSGIVING}), D(2026, 11, 25))
        self.assertEqual(add_business_days(D(2026, 11, 23), -1), D(2026, 11, 20))
        self.assertEqual(add_business_days(D(2026, 12, 28), -2, {CHRISTMAS}), D(2026, 12, 23))
        self.assertEqual(add_business_days(D(2026, 11, 22), -1), D(2026, 11, 20))

    def test_zero(self):
        self.assertEqual(add_business_days(D(2026, 11, 21), 0), D(2026, 11, 21))
        self.assertEqual(add_business_days(THANKSGIVING, 0, {THANKSGIVING}), THANKSGIVING)

    def test_holidays_as_a_list(self):
        self.assertEqual(add_business_days(D(2026, 11, 25), 1, [THANKSGIVING]), D(2026, 11, 27))


if __name__ == "__main__":
    unittest.main()
