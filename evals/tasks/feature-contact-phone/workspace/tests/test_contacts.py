import unittest

from contacts import Contact, ValidationError, from_dict, to_dict, validate_contact


class ContactsTest(unittest.TestCase):
    def test_round_trip(self):
        contact = Contact("Ana", "ana@example.com", ["vip"])
        self.assertEqual(from_dict(to_dict(contact)), contact)

    def test_bad_email(self):
        with self.assertRaises(ValidationError) as caught:
            validate_contact(Contact("Ana", "nope"))
        self.assertEqual(caught.exception.field, "email")

    def test_stored_form(self):
        self.assertEqual(to_dict(Contact("Bo", "bo@example.com")), {"name": "Bo", "email": "bo@example.com"})


if __name__ == "__main__":
    unittest.main()
