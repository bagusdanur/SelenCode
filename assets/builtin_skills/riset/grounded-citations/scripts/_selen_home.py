"""Resolve SelenCode data directory safely for local citation store."""
import os
from pathlib import Path

def get_selen_home() -> Path:
    custom = os.environ.get("SELENCODE_DATA_DIR", "").strip()
    if custom:
        return Path(custom) / "selencode"
    xdg = os.environ.get("XDG_DATA_HOME", "").strip()
    if xdg:
        return Path(xdg) / "selencode"
    return Path.home() / ".local" / "share" / "selencode"
