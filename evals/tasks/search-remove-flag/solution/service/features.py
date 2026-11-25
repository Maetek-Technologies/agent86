"""Feature flags, read once at start-up from config/flags.json."""

import json
import pathlib

_FLAGS = json.loads((pathlib.Path(__file__).resolve().parent.parent / "config" / "flags.json").read_text(encoding="utf-8"))

ENABLE_DARK_MODE = _FLAGS["enable_dark_mode"]
ENABLE_BULK_EDIT = _FLAGS["enable_bulk_edit"]
