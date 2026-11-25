import dataclasses
import pathlib
import unittest

from contacts import Contact, ValidationError, from_dict, to_dict, validate_contact

ROOT = pathlib.Path(__file__).resolve().parent.parent


class Hidden(unittest.TestCase):
    def test_the_field(self):
        names = [f.name for f in dataclasses.fields(Contact)]
        self.assertEqual(names, ["name", "email", "tags", "phone"])
        self.assertIsNone(Contact("Ana", "ana@example.com").phone)

    def test_valid_phones(self):
        for phone in ("+44 20 7946 0958", "555-0100-12", "1234567", "+123456789012345", "020 7946-0958"):
            validate_contact(Contact("Ana", "ana@example.com", phone=phone))
        validate_contact(Contact("Ana", "ana@example.com"))

    def test_invalid_phones(self):
        for phone in ("123456", "+1234567890123456", "12a4567", "44+2079460958", "(020) 7946 0958",
                      "", "++4420794609"):
            with self.assertRaises(ValidationError, msg=phone) as caught:
                validate_contact(Contact("Ana", "ana@example.com", phone=phone))
            self.assertEqual(caught.exception.field, "phone", phone)

    def test_other_rules_first_still_apply(self):
        with self.assertRaises(ValidationError) as caught:
            validate_contact(Contact("Ana", "nope", phone="1234567"))
        self.assertEqual(caught.exception.field, "email")

    def test_stored_form(self):
        self.assertEqual(to_dict(Contact("Bo", "bo@example.com")), {"name": "Bo", "email": "bo@example.com"})
        stored = to_dict(Contact("Bo", "bo@example.com", phone="+1 555 0100 12"))
        self.assertEqual(stored["phone"], "+1 555 0100 12")
        self.assertEqual(from_dict(stored).phone, "+1 555 0100 12")
        old = from_dict({"name": "Cy", "email": "cy@example.com", "tags": ["x"]})
        self.assertIsNone(old.phone)
        self.assertEqual(old.tags, ["x"])

    def test_the_existing_tests_pass(self):
        suite = unittest.defaultTestLoader.discover(str(ROOT / "tests"), top_level_dir=str(ROOT))
        result = unittest.TestResult()
        suite.run(result)
        self.assertGreater(result.testsRun, 0)
        self.assertEqual(result.failures + result.errors, [])


if __name__ == "__main__":
    unittest.main()
