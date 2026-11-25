"""Contacts: the model, its validation, and its stored form."""

from .model import Contact
from .serialize import from_dict, to_dict
from .validate import ValidationError, validate_contact

__all__ = ["Contact", "ValidationError", "from_dict", "to_dict", "validate_contact"]
