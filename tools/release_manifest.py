#!/usr/bin/env python3
"""Describe a trusted build for the fixed, schema-compatible update protocol."""
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

root = Path(__file__).resolve().parent.parent
version = re.search(r'^version = "([0-9]+\.[0-9]+\.[0-9]+)"$',
                    (root / 'Cargo.toml').read_text(), re.MULTILINE).group(1)
binary = Path(sys.argv[1]).resolve()
actual = subprocess.check_output([str(binary), '--version'], text=True).strip()
if actual != f'oyasys {version}':
    raise SystemExit(f'Binary version {actual!r} does not match Cargo version {version}')
archive = root / 'dist' / 'oyasys-linux-x86_64.tar.gz'
manifest = {
    'protocol': 1,
    'version': version,
    'archive': archive.name,
    'sha256': hashlib.sha256(archive.read_bytes()).hexdigest(),
    'platform': 'linux-x86_64',
    'database_schema': 1,
}
(root / 'dist' / 'oyasys-update.json').write_text(json.dumps(manifest, indent=2) + '\n')
