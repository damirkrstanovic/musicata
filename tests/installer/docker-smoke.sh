#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
# Exercise the Docker installer without allowing its fixed production names or
# paths to escape a disposable test namespace on the Docker host.
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
archive=${MUSICATA_RELEASE_ARCHIVE:-/tmp/musicata-release-1.0.9/musicata-x86_64-linux.tar.gz}
port=${MUSICATA_INSTALLER_TEST_PORT:-19930}
[[ -f $archive ]] || { echo "missing release archive: $archive" >&2; exit 2; }
command -v docker >/dev/null || { echo 'docker is required' >&2; exit 2; }
docker info >/dev/null
if ss -ltn "sport = :$port" | grep -q ":$port"; then
    echo "test port $port is already in use" >&2
    exit 2
fi

scratch=$(mktemp -d "${TMPDIR:-/tmp}/musicata-installer-it.XXXXXX")
id=$(basename "$scratch" | tr '[:upper:]' '[:lower:]' | tr -cd '[:alnum:]')
prefix="musicata-installer-it-$id"
fixture="$prefix-server"
failing="$prefix-fail"
outer="$prefix-outer"
varlib="$scratch/var-lib"
data="$varlib/musicata"
library="$scratch/library"
config="$scratch/etc-musicata"
backups="$scratch/backups"
mkdir -p "$data" "$library" "$config" "$backups" "$scratch/context"
chmod 0777 "$data" "$library"

cleanup() {
    rc=$?
    if [ "$rc" -ne 0 ]; then
        echo "installer smoke failed; namespaced Docker diagnostics:" >&2
        docker ps -a --filter "name=^/${prefix}-" --format '{{.Names}} {{.Status}}' >&2 || true
        docker logs "$prefix-musicata" >&2 || true
        docker logs "$prefix-outer" >&2 || true
    fi
    docker ps -aq --filter "name=^/${prefix}-" | xargs -r docker rm -f >/dev/null 2>&1 || true
    # The installer runs as root in the outer container, so backups in this
    # directory may be root-owned. Clear only this mounted scratch before its
    # outer image is removed; the command has no host path interpolation.
    docker run --rm --user 0 -v "$scratch:/it" "$outer" /bin/sh -c 'find /it -mindepth 1 -delete' >/dev/null 2>&1 || true
    docker image rm -f "$outer" "$failing" "$fixture" >/dev/null 2>&1 || true
    rmdir "$scratch" || true
}
trap cleanup EXIT

tar -xzf "$archive" -C "$scratch/context"
cp "$repo/packaging/install.py" "$scratch/context/install.py"
cat >"$scratch/context/Dockerfile.fixture" <<'EOF'
FROM debian:bookworm-slim
RUN useradd --system --no-create-home --uid 10001 musicata
COPY musicata-x86_64-linux/musicata-server /usr/local/bin/musicata-server
RUN mkdir /data && chown musicata:musicata /data
ENV MUSICATA_DATABASE=/data/musicata.db MUSICATA_ADDR=0.0.0.0:3030 MUSICATA_LIBRARY=/music
VOLUME /data
USER musicata
ENTRYPOINT ["musicata-server"]
EOF
cat >"$scratch/context/Dockerfile.fail" <<EOF
FROM $fixture
USER root
ENTRYPOINT ["/bin/sh", "-c", "exit 1"]
EOF
cat >"$scratch/context/Dockerfile.outer" <<'EOF'
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl docker.io python3 util-linux && rm -rf /var/lib/apt/lists/* \
 && useradd --system --no-create-home --uid 10001 musicata && mkdir -p /run/systemd/system /it/bin
COPY install.py /it/install.py
COPY docker-wrapper /it/bin/docker
COPY systemctl-stub /it/bin/systemctl
COPY dpkg-query-stub /it/bin/dpkg-query
RUN chmod 755 /it/bin/*
ENV PATH=/it/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
EOF
cat >"$scratch/context/systemctl-stub" <<'EOF'
#!/bin/sh
[ "${1:-}" = is-active ] && exit 3
exit 0
EOF
cat >"$scratch/context/dpkg-query-stub" <<'EOF'
#!/bin/sh
# Dependencies are deliberately supplied by the disposable outer image.
printf installed
EOF
cat >"$scratch/context/docker-wrapper" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
real=/usr/bin/docker
prefix=${IT_PREFIX:?}
fixture=${IT_FIXTURE:?}
failing=${IT_FAIL_IMAGE:?}
data=${IT_DATA:?}
library=${IT_LIBRARY:?}
log=${IT_DOCKER_LOG:?}
printf '%q ' "$@" >>"$log"; printf '\n' >>"$log"
map_name() {
    case "$1" in
        musicata|musicata-ml|musicata-previous-*|musicata-ml-previous-*) printf '%s-%s\n' "$prefix" "$1" ;;
        "$prefix"-*) printf '%s\n' "$1" ;;
        *) echo "blocked Docker resource: $1" >&2; exit 125 ;;
    esac
}
is_release_ref() { [[ $1 == ghcr.io/damirkrstanovic/musicata-*:* ]]; }
is_fixture_image() {
    is_release_ref "$1" && return 0
    [ "$1" = "$fixture" ] && return 0
    [ "$1" = "$("$real" image inspect --format '{{.Id}}' "$fixture")" ]
}
case "${1:-}" in
    pull)
        is_release_ref "${2:-}" || { echo 'blocked Docker pull' >&2; exit 125; }
        "$real" image inspect "$fixture" >/dev/null
        echo "fixture image satisfies $2"
        ;;
    image)
        [ "${2:-}" = inspect ] || { echo 'blocked Docker image operation' >&2; exit 125; }
        args=("${@:3}"); last=$(( ${#args[@]} - 1 ))
        is_release_ref "${args[$last]}" || { echo 'blocked Docker image reference' >&2; exit 125; }
        args[$last]=$fixture
        exec "$real" image inspect "${args[@]}"
        ;;
    inspect|stop|start)
        [ $# -eq 2 ] || { echo "blocked Docker $1 arguments" >&2; exit 125; }
        exec "$real" "$1" "$(map_name "$2")"
        ;;
    rm)
        if [ $# -eq 2 ]; then exec "$real" rm "$(map_name "$2")"; fi
        if [ $# -eq 3 ] && [ "$2" = -f ]; then exec "$real" rm -f "$(map_name "$3")"; fi
        echo 'blocked Docker rm arguments' >&2; exit 125
        ;;
    rename)
        [ $# -eq 3 ] || { echo 'blocked Docker rename arguments' >&2; exit 125; }
        exec "$real" rename "$(map_name "$2")" "$(map_name "$3")"
        ;;
    run)
        args=("${@:2}")
        out=()
        name=''
        for ((i=0; i<${#args[@]}; i++)); do
            arg=${args[$i]}
            case "$arg" in
                --name) ((++i)); name=$(map_name "${args[$i]}"); out+=(--name "$name") ;;
                --name=*) name=$(map_name "${arg#--name=}"); out+=(--name="$name") ;;
                -v) ((++i)); mount=${args[$i]}; mount=${mount/#\/var\/lib\/musicata:/$data:}; mount=${mount/#\/var\/lib\/musicata-ml:/$data-ml:}; mount=${mount/#\/srv\/music:/$library:}; out+=(-v "$mount") ;;
                /var/lib/musicata:*|/var/lib/musicata-ml:*|/srv/music:*) mount=${arg/#\/var\/lib\/musicata:/$data:}; mount=${mount/#\/var\/lib\/musicata-ml:/$data-ml:}; mount=${mount/#\/srv\/music:/$library:}; out+=("$mount") ;;
                *) out+=("$arg") ;;
            esac
        done
        [ -n "$name" ] || { echo 'blocked unnamed Docker run' >&2; exit 125; }
        last=$(( ${#out[@]} - 1 )); is_fixture_image "${out[$last]}" || { echo 'blocked Docker run image' >&2; exit 125; }
        out[$last]=$fixture
        if [ "${IT_FAIL_NEW:-0}" = 1 ] && [ "$name" = "$prefix-musicata" ]; then out[$last]=$failing; fi
        exec "$real" run "${out[@]}"
        ;;
    *) echo "blocked Docker operation: ${1:-}" >&2; exit 125 ;;
esac
EOF

docker build -q -f "$scratch/context/Dockerfile.fixture" -t "$fixture" "$scratch/context" >/dev/null
docker build -q -f "$scratch/context/Dockerfile.fail" -t "$failing" "$scratch/context" >/dev/null
docker build -q -f "$scratch/context/Dockerfile.outer" -t "$outer" "$scratch/context" >/dev/null

run_installer() {
    docker run --rm --name "$outer" --network host \
        -v /var/run/docker.sock:/var/run/docker.sock \
        -v "$scratch:/it/host" \
        -v "$config:/etc/musicata" -v "$varlib:/var/lib" \
        -v "$library:/srv/music:ro" -v "$backups:/var/backups/musicata" \
        -e IT_PREFIX="$prefix" -e IT_FIXTURE="$fixture" -e IT_FAIL_IMAGE="$failing" \
        -e IT_DATA="$data" -e IT_LIBRARY="$library" -e IT_DOCKER_LOG=/it/host/docker.log \
        -e IT_FAIL_NEW="${IT_FAIL_NEW:-0}" \
        "$outer" python3 /it/install.py "$@"
}

health() {
    curl -fsS --retry 20 --retry-connrefused --retry-delay 1 --max-time 5 "$1"
}

run_installer --mode docker --version 1.0.9 --library /srv/music --port "$port" --yes
health "http://127.0.0.1:$port/api/health" >"$scratch/first-health.json"
printf sentinel >"$data/installer-persistence-sentinel"

run_installer --upgrade --yes
health "http://127.0.0.1:$port/api/health" >"$scratch/upgrade-health.json"
test "$(cat "$data/installer-persistence-sentinel")" = sentinel

if IT_FAIL_NEW=1 run_installer --upgrade --yes; then
    echo 'expected failed upgrade to return non-zero' >&2
    exit 1
fi
health "http://127.0.0.1:$port/api/health" >"$scratch/rollback-health.json"
test "$(cat "$data/installer-persistence-sentinel")" = sentinel
test "$(docker ps -q --filter "name=^/${prefix}-musicata$")" != ''
test -z "$(docker ps -aq --filter "name=^/${prefix}-musicata-previous-")"

echo "PASS: Docker installer first install, upgrade persistence, and failed-upgrade rollback"
echo "fixture: $fixture; port: $port; wrapper log: $scratch/docker.log"
