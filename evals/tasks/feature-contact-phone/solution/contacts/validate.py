"""Rules a contact must meet before it is saved."""

import re

EMAIL = re.compile(r"^[^@\s]+@[^@\s]+\.[^@\s]+$")
PHONE = re.compile(r"^\+?[0-9 -]+$")


class ValidationError(ValueError):
    """A contact broke a rule; ``field`` names which field."""

    def __init__(self, field, message):
        super().__init__(f"{field}: {message}")
        self.field = field


def validate_contact(contact):
    """Raise ValidationError for the first rule ``contact`` breaks."""
    if not contact.name.strip():
        raise ValidationError("name", "must not be empty")
    if not EMAIL.match(contact.email):
        raise ValidationError("email", "is not an email address")
    for tag in contact.tags:
        if not tag or tag != tag.strip().lower():
            raise ValidationError("tags", f"{tag!r} must be lower case with no spaces around it")
    if contact.phone is not None:
        digits = sum(c.isdigit() for c in contact.phone)
        if not PHONE.match(contact.phone) or not 7 <= digits <= 15:
            raise ValidationError("phone", "is not a phone number")
