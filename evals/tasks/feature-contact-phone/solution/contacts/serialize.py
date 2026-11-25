"""Contacts as plain dicts, the form they are stored in."""

from .model import Contact


def to_dict(contact):
    data = {"name": contact.name, "email": contact.email}
    if contact.tags:
        data["tags"] = list(contact.tags)
    if contact.phone is not None:
        data["phone"] = contact.phone
    return data


def from_dict(data):
    return Contact(name=data["name"], email=data["email"], tags=list(data.get("tags", [])),
                   phone=data.get("phone"))
