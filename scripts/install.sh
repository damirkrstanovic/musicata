#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
# Bootstrap the standard-library installer, from a checkout or extracted release.
set -euo pipefail
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
installer="$script_dir/../packaging/install.py"
[[ -f "$installer" ]] || installer="$script_dir/install.py"
[[ -f "$installer" ]] || { echo 'install.py is missing; use a complete release archive or source checkout.' >&2; exit 1; }
if ! command -v python3 >/dev/null; then
    echo 'Missing installer dependency: Python 3.9 or newer.' >&2
    readonly_mode=false
    assume_yes=false
    for arg in "$@"; do
        case "$arg" in --check|--dry-run|--help|-h) readonly_mode=true;; --yes) assume_yes=true;; esac
    done
    # /etc/os-release is the root-owned operating system identity file.
    # shellcheck source=/dev/null
    . /etc/os-release
    distro=" ${ID:-} ${ID_LIKE:-} "
    case "$distro" in
        *' ubuntu '*|*' debian '*) bootstrap=(apt-get install -y python3); refresh=(apt-get update);;
        *' arch '*) bootstrap=(pacman -Syu --needed --noconfirm python); refresh=();;
        *' fedora '*) bootstrap=(dnf install -y python3); refresh=();;
        *) echo 'Unsupported distribution. Install Python 3.9+ before running this installer.' >&2; exit 1;;
    esac
    printf 'Required command: sudo %s\n' "${bootstrap[*]}" >&2
    if $readonly_mode || [[ $EUID != 0 ]]; then exit 1; fi
    if ! $assume_yes; then
        [[ -t 0 ]] || { echo 'Use --yes for a non-interactive installation.' >&2; exit 1; }
        read -r -p 'Install Python with the distribution package manager? [y/N] ' answer
        [[ "$answer" == y || "$answer" == Y ]] || exit 1
    fi
    if ((${#refresh[@]})); then "${refresh[@]}"; fi
    "${bootstrap[@]}"
fi
python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3,9) else "Python 3.9 or newer is required")'
exec python3 "$installer" "$@"
