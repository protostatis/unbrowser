"""scripts/unbrowse.py — backward-compat shim.

The real implementation lives in ``python/unbrowse/__init__.py`` (the
PyPI-publishable package). This file exists so the SKILL.md pattern and
existing dev scripts keep working without `pip install`:

    sys.path.insert(0, "/path/to/repo/scripts")
    from unbrowse import Client

It's a thin re-export — no client logic lives here.
"""

from __future__ import annotations

import sys
from pathlib import Path

_PKG_DIR = Path(__file__).resolve().parent.parent / "python"
if str(_PKG_DIR) not in sys.path:
    sys.path.insert(0, str(_PKG_DIR))

# Python has already registered us as 'unbrowse' in sys.modules to run this
# code. If we don't pop the entry, the next `from unbrowse import ...`
# would short-circuit back to this half-loaded module instead of finding
# the real package on the path we just added. After the import lands, the
# package re-registers itself under the same name — caller is none the
# wiser.
sys.modules.pop("unbrowse", None)
from unbrowse import (  # noqa: E402  -- intentional: shim must run path setup before import
    Client,
    UnbrowseError,
    __version__,
    find_binary,
    navigate,
)

__all__ = ["Client", "UnbrowseError", "find_binary", "navigate", "__version__"]
