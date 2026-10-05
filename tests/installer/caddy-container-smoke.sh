#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
# Real prebuilt Caddy container, isolated pod network/storage, local test CA only.
set -euo pipefail
kubectl() { command kubectl --context "${MUSICATA_KUBE_CONTEXT:-homelab-k3s}" "$@"; }
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
namespace="musicata-caddy-test-$(date +%s)-$$"
scratch=$(mktemp -d /tmp/musicata-caddy-container.XXXXXX)
cleanup() {
    kubectl -n "$namespace" logs proxy -c caddy >"$scratch/caddy.log" 2>&1 || true
    kubectl delete namespace "$namespace" --ignore-not-found --wait=true --timeout=60s >/dev/null 2>&1 || true
    echo "Caddy container logs: $scratch/caddy.log"
}
trap cleanup EXIT
kubectl create namespace "$namespace" >/dev/null
PYTHONDONTWRITEBYTECODE=1 python3 - "$root" "$namespace" "${1:-}" >"$scratch/resources.json" <<'PY'
import importlib.util,json,os,sys
from pathlib import Path
root=Path(sys.argv[1]);namespace=sys.argv[2]
spec=importlib.util.spec_from_file_location('installer',root/'packaging/install.py')
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
cfg=m.options(['--with','caddy','--tls-domain','music.example.org'])
cloudflare=sys.argv[3]=='--cloudflare'
config=m.caddy_text(cfg).replace('music.example.org {','music.example.org {\n    tls internal')
configmap={'apiVersion':'v1','kind':'ConfigMap','metadata':{'name':'fixture','namespace':namespace},'data':{'Caddyfile':config,'smoke.py':(root/'tests/installer/caddy-smoke.py').read_text()}}
pod={'apiVersion':'v1','kind':'Pod','metadata':{'name':'proxy','namespace':namespace},'spec':{
 'restartPolicy':'Always','securityContext':{'runAsUser':10001,'runAsGroup':10001,'runAsNonRoot':True,'fsGroup':10001},
 'volumes':[{'name':'fixture','configMap':{'name':'fixture'}},{'name':'data','emptyDir':{}},{'name':'config','emptyDir':{}}],
 'containers':[
  {'name':'backend','image':'python:3.14-alpine','command':['python','/work/smoke.py','--backend-only'],
   'securityContext':{'allowPrivilegeEscalation':False,'capabilities':{'drop':['ALL']}},
   'volumeMounts':[{'name':'fixture','mountPath':'/work','readOnly':True},{'name':'data','mountPath':'/caddy-data','readOnly':True}]},
  {'name':'caddy','image':'caddy:2-alpine','command':['caddy','run','--config','/etc/caddy/Caddyfile','--adapter','caddyfile'],
   'securityContext':{'capabilities':{'drop':['ALL'],'add':['NET_BIND_SERVICE']}},
   'resources':{'requests':{'cpu':'100m','memory':'64Mi'},'limits':{'cpu':'1','memory':'256Mi'}},
   'volumeMounts':[{'name':'fixture','mountPath':'/etc/caddy','readOnly':True},{'name':'data','mountPath':'/data'},{'name':'config','mountPath':'/config'}],
   'readinessProbe':{'tcpSocket':{'port':443},'periodSeconds':2,'timeoutSeconds':3}}
 ]}}
if cloudflare:
    cfg.update(tls_dns='cloudflare')
    configmap['data']['DNS-Caddyfile']=m.caddy_text(cfg)
    pod['spec']['initContainers']=[
      {'name':'validate','image':os.environ.get('MUSICATA_CADDY_IMAGE',m.CLOUDFLARE_IMAGE),
       'env':[{'name':'CLOUDFLARE_API_TOKEN','value':'a'*40}],
       'command':['sh','-ec','caddy version | grep -q "^'+m.CADDY_VERSION+' "; caddy list-modules | grep -qx dns.providers.cloudflare; caddy validate --config /etc/caddy/DNS-Caddyfile --adapter caddyfile'],
       'volumeMounts':[{'name':'fixture','mountPath':'/etc/caddy','readOnly':True}]}
    ]
    pod['spec']['containers'][1]['image']=os.environ.get('MUSICATA_CADDY_IMAGE',m.CLOUDFLARE_IMAGE)
print(json.dumps({'apiVersion':'v1','kind':'List','items':[configmap,pod]}))
PY
kubectl apply -f "$scratch/resources.json" >/dev/null
kubectl -n "$namespace" wait --for=condition=Ready pod/proxy --timeout=900s >/dev/null
kubectl -n "$namespace" exec proxy -c backend -- python /work/smoke.py --check-only --ca /caddy-data/caddy/pki/authorities/local/root.crt
before=$(kubectl -n "$namespace" exec proxy -c caddy -- sha256sum /data/caddy/pki/authorities/local/root.crt)
kubectl -n "$namespace" exec proxy -c caddy -- sh -c 'kill -TERM 1'
deadline=$((SECONDS+45))
until kubectl -n "$namespace" get pod proxy -o json | python3 -c 'import json,sys; s=json.load(sys.stdin)["status"].get("containerStatuses",[]); sys.exit(not any(c["name"]=="caddy" and c["restartCount"]>0 and c["ready"] for c in s))'; do
    ((SECONDS < deadline)) || { echo 'Caddy container did not restart' >&2; exit 1; }
    sleep 1
done
after=$(kubectl -n "$namespace" exec proxy -c caddy -- sha256sum /data/caddy/pki/authorities/local/root.crt)
[[ "$before" == "$after" ]] || { echo 'Certificate authority did not persist' >&2; exit 1; }
kubectl -n "$namespace" exec proxy -c backend -- python /work/smoke.py --check-only --ca /caddy-data/caddy/pki/authorities/local/root.crt
echo 'Non-root Caddy container and certificate persistence passed'
