"""The application's settings."""

import configparser

DEFAULT_SETTINGS = "settings.ini"


def load_settings(path=DEFAULT_SETTINGS):
    """Read the application's settings.

    With no ``path``, the settings are the settings.ini that ships next to
    this module, wherever the program is started from. A ``path`` given is
    used as it is.
    """
    parser = configparser.ConfigParser()
    parser.read(path)
    return {
        "database": parser["database"]["url"],
        "workers": parser.getint("app", "workers"),
        "name": parser.get("app", "name"),
    }
