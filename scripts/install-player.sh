#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
installer="$script_dir/../packaging/install_player.py"
[[ -f "$installer" ]] || installer="$script_dir/install_player.py"
[[ -f "$installer" ]] || { echo 'Use a complete source checkout or release archive.' >&2; exit 1; }
python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3,9) else "Python 3.9 or newer is required")'
exec python3 "$installer" "$@"
