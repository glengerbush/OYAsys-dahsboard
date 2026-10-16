#!/usr/bin/env python3
"""Copy dependency notices for the Linux build into a release staging directory."""
import json
from pathlib import Path
import shutil
import subprocess
import sys

out = Path(sys.argv[1])
metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--locked', '--format-version', '1', '--filter-platform', 'x86_64-unknown-linux-gnu'], text=True))
nodes = {node['id']:node for node in metadata['resolve']['nodes']}
packages = {package['id']:package for package in metadata['packages']}
seen = set()
queue = [metadata['resolve']['root']]
while queue:
    current = queue.pop()
    if current in seen:
        continue
    seen.add(current)
    queue.extend(dep['pkg'] for dep in nodes[current]['deps'])
seen.discard(metadata['resolve']['root'])
notices = ['Third-party dependencies used by the Linux executable/build. Licenses and notices are copied verbatim below. Vendored browser library notice: LICENSE-uPlot.\n']
for key in sorted(seen):
    package = packages[key]
    root = Path(package['manifest_path']).parent
    destination = out / f"{package['name']}-{package['version']}"
    files = [p for p in root.iterdir() if p.is_file() and p.name.upper().startswith(('LICENSE', 'COPYING', 'NOTICE'))]
    if package.get('license_file'):
        files.append(root / package['license_file'])
    if not files:
        raise SystemExit(f"Missing dependency license file: {package['name']}")
    destination.mkdir(parents=True, exist_ok=True)
    for source in set(files):
        shutil.copyfile(source, destination/source.name)
    notices.append(f"{package['name']} {package['version']} — {package.get('license') or 'see license file'} — {package.get('repository') or ''}")
out.mkdir(parents=True, exist_ok=True)
(out/'INDEX.txt').write_text('\n'.join(notices)+'\n')
