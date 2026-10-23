#!/usr/bin/env python3
"""Remove machine-local Cargo paths from cargo-about's generated metadata only."""
import json
import os
from pathlib import Path
import sys

root = Path(__file__).resolve().parents[1]
cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
replacements = [(str(root), "workspace")]
for registry in (cargo_home / "registry" / "src").glob("*"):
    if registry.is_dir():
        replacements.append((str(registry), "registry"))
# Longest prefixes first. Keep license texts and package/version identity intact.
replacements.sort(key=lambda pair: len(pair[0]), reverse=True)

def clean(value):
    if isinstance(value, str):
        for old, new in replacements:
            value = value.replace(old, new).replace(old.replace("\\", "/"), new)
        return value
    if isinstance(value, list):
        return [clean(item) for item in value]
    if isinstance(value, dict):
        return {key: clean(item) for key, item in value.items()}
    return value

path = Path(sys.argv[1]) if len(sys.argv) > 1 else root / "THIRD-PARTY-LICENSES.json"
data = json.loads(path.read_text(encoding="utf-8"))
path.write_text(json.dumps(clean(data), ensure_ascii=False, separators=(",", ":")) + "\n", encoding="utf-8")
