"""The app's greeting line."""

from utils import clamp, initials, title_case


def greeting(name, unread):
    shown = clamp(unread, 0, 99)
    return f"{title_case(name)} ({initials(name)}): {shown} unread"
