#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Disposable native-installer integration test.  This deliberately uses a fake
# systemctl only because ordinary Kubernetes pods do not run PID 1 systemd; its
# restart action launches the real static Musicata binary as the configured
# service account.  It does not test systemd boot integration.
set -euo pipefail

namespace="musicata-installer-test-$(date +%s)-$$"
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
log_dir=${MUSICATA_INSTALLER_SMOKE_LOG_DIR:-/tmp}
trap 'kubectl delete namespace "$namespace" --ignore-not-found --wait=true --timeout=120s >/dev/null 2>&1 || true' EXIT

kubectl create namespace "$namespace" --dry-run=client -o yaml | kubectl apply -f - >/dev/null

run_one() {
    local name=$1 image=$2
    local pod="native-${name}"
    kubectl -n "$namespace" run "$pod" --image="$image" --restart=Never --command -- sleep 900 >/dev/null
    kubectl -n "$namespace" wait --for=condition=Ready "pod/$pod" --timeout=90s >/dev/null
    kubectl -n "$namespace" exec "$pod" -- mkdir -p /work/bin /run/systemd/system /run/lock /srv/music
    # `install.sh` falls back to an adjacent install.py in extracted releases.
    kubectl -n "$namespace" cp "$root/packaging/install.py" "$pod:/work/install.py"
    kubectl -n "$namespace" cp "$root/scripts/install.sh" "$pod:/work/install.sh"
    kubectl -n "$namespace" exec -i "$pod" -- sh -c 'cat > /work/bin/systemctl && chmod +x /work/bin/systemctl' <<'SYSTEMCTL'
#!/bin/sh
set -eu
action="${1:-}"; shift || true
case "$action" in
  is-active|cat) exit 3 ;;
  daemon-reload|enable|disable) exit 0 ;;
  stop)
    test ! -f /run/musicata.pid || kill "$(cat /run/musicata.pid)" 2>/dev/null || true
    rm -f /run/musicata.pid
    ;;
  start|restart)
    if test "${1:-}" = musicata.service; then
      test ! -f /run/musicata.pid || kill "$(cat /run/musicata.pid)" 2>/dev/null || true
      runuser -u musicata -- env MUSICATA_DATABASE=/var/lib/musicata/musicata.db MUSICATA_ADDR=0.0.0.0:3030 MUSICATA_LIBRARY=/srv/music /opt/musicata/current/musicata-server >/run/musicata.log 2>&1 &
      echo $! >/run/musicata.pid
    fi
    ;;
esac
SYSTEMCTL
    kubectl -n "$namespace" exec "$pod" -- sh -c 'printf "#!/bin/sh\nexit 0\n" >/work/bin/systemd-analyze; chmod +x /work/bin/systemd-analyze /work/install.sh'

    # The installer downloads release metadata and the archive itself, then
    # verifies GitHub's published asset digest before activation.
    # shellcheck disable=SC2016 # $PATH expands in the pod, not in this harness.
    kubectl -n "$namespace" exec "$pod" -- sh -c 'PATH=/work/bin:$PATH /work/install.sh --mode native --version 1.0.9 --yes'
    # shellcheck disable=SC2016 # $PATH and $(cat ...) expand in the pod.
    kubectl -n "$namespace" exec "$pod" -- sh -c 'echo persisted >/var/lib/musicata/installer-smoke-sentinel; PATH=/work/bin:$PATH /work/install.sh --upgrade --version 1.0.9 --yes; test "$(cat /var/lib/musicata/installer-smoke-sentinel)" = persisted; curl -fsS http://127.0.0.1:3030/api/health >/dev/null'
    echo "PASS $name"
}

run_logged() {
    local name=$1 image=$2
    local log="$log_dir/musicata-installer-native-${name}.log"
    rm -f "$log"
    set +e
    (set -e; run_one "$name" "$image") >"$log" 2>&1
    local status=$?
    set -e
    if test "$status" -ne 0; then
        {
            printf '\n--- pod logs ---\n'
            kubectl -n "$namespace" logs "native-$name" --all-containers=true 2>&1 || true
            printf '\n--- pod describe ---\n'
            kubectl -n "$namespace" describe "pod/native-$name" 2>&1 || true
        } >>"$log"
        cat "$log" >&2
        return 1
    fi
    cat "$log"
}

run_logged debian debian:trixie-slim
run_logged ubuntu ubuntu:26.04
run_logged fedora fedora:44
run_logged arch archlinux:latest
