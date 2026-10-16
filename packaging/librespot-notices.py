#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Collect license declarations and shipped license/NOTICE files from the locked build."""
import json
from pathlib import Path
import re
import sys

metadata = json.loads(Path(sys.argv[1]).read_text())
resolved = {node['id'] for node in metadata['resolve']['nodes']}
sections = ['Librespot 0.8.0 and dependency notices\n\n'
            'Source: https://github.com/librespot-org/librespot/tree/d36f9f1907e8cc9d68a93f8ebc6b627b1bf7267d\n'
            'Registry crate sources (including MPL-2.0 dependencies) are available at\n'
            'https://crates.io/crates/NAME/VERSION for each exact version listed below.\n']
for package in sorted(metadata['packages'], key=lambda p: (p['name'], p['version'])):
    if package['id'] not in resolved:
        continue
    license = package['license'] or package['license_file']
    if not license:
        raise SystemExit('Missing license declaration: ' + package['name'])
    root = Path(package['manifest_path']).parent
    files = [p for p in root.iterdir() if p.is_file() and
             re.match(r'^(licen[cs]e|copying|notice)([-_.].*)?$', p.name, re.I)]
    if package['license_file']:
        files.append(root / package['license_file'])
    # The upstream workspace's MIT license covers its member crates too.
    if package['name'].startswith('librespot') and not files:
        files = [root.parent / 'LICENSE']
    sections.append(f"\n{package['name']} {package['version']} — {license}\n")
    for path in sorted(set(files)):
        sections.append(path.read_text() + '\n')
Path(sys.argv[2]).write_text('\n'.join(sections))
