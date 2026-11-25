"""The contact record."""

from dataclasses import dataclass, field


@dataclass
class Contact:
    name: str
    email: str
    tags: list = field(default_factory=list)
