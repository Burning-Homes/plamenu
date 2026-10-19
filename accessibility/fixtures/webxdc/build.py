"""Rebuild the small, deterministic, source-visible browser test package."""
from pathlib import Path
from zipfile import ZipFile, ZipInfo, ZIP_STORED

root = Path(__file__).resolve().parent
with ZipFile(root / "a11y.xdc", "w", compression=ZIP_STORED) as package:
    for name in ("index.html", "app.js", "style.css", "manifest.toml"):
        entry = ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
        entry.external_attr = 0o100644 << 16
        package.writestr(entry, (root / name).read_bytes())
