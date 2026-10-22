#!/bin/sh
# Legacy Node smoke check for `vexor shim` (seed of plan step 2.8).
#
# Usage (from the repo root, with a static Linux build in ./dist):
#   for v in 8 12 16 24; do
#     docker run --rm -v "$PWD/dist":/v:ro -v "$PWD/tests/legacy":/t:ro node:$v-alpine /t/node-smoke.sh
#   done
#
# Checks: ready via NODE_CHANNEL_FD, natural exit with an IPC channel open,
# graceful SIGTERM stop.
N=$(command -v node); D=$(mktemp -d)
spec() { printf '{"name":"t","exec":"%s","args":["-e","%s"],"cwd":"%s","node_ipc":true,"logs":{"out":"%s/o","err":"%s/e"},"policy":%s}' "$N" "$1" "$D" "$D" "$D" "$2" > $D/s.json; }
spec "setTimeout(function(){process.send(\\\"ready\\\")},200);setTimeout(function(){process.exit(0)},400)" '{"ready_timeout_ms":5000,"stop_exit_codes":[0]}'
R1=$(/v/vexor shim --spec $D/s.json | sed -n 's/.*"state":"\([a-z]*\)".*/\1/p' | tr '\n' ' ')
spec "console.log(1)" '{"autorestart":false}'
T0=$(date +%s); /v/vexor shim --spec $D/s.json >/dev/null; R2="exit=$? in $(( $(date +%s) - T0 ))s"
spec "process.on(\\\"SIGTERM\\\",function(){console.log(\\\"term\\\");process.exit(0)});console.log(\\\"up\\\");setInterval(function(){},1000)" '{}'
/v/vexor shim --spec $D/s.json >/dev/null & P=$!; while ! grep -q up $D/o 2>/dev/null; do sleep 0.1; done; kill -TERM $P; wait $P; R3="exit=$? log=$(tr '\n' ' ' < $D/o)"
echo "node $(node -v): ready:[$R1] natural-exit:[$R2] sigterm:[$R3]"
