#!/bin/sh
# Legacy Node runtime checks for `vexor shim` (plan step 2.8).
#
# Runs inside a node:<version>-alpine container with a static vexor binary
# mounted at /v/vexor. Exits non-zero if any check fails.
#
#   for v in 8 10 12 14 16 18 20 22 24; do
#     docker run --rm -v "$PWD/dist":/v:ro -v "$PWD/tests/legacy":/t:ro \
#       node:$v-alpine /t/node-smoke.sh
#   done
#
# JavaScript below uses only single quotes so it can sit inside JSON strings.

set -u
VEXOR=/v/vexor
NODE=$(command -v node)
VERSION=$(node -v)
FAILED=0
D=$(mktemp -d)

pass() { echo "ok    $VERSION  $1"; }
fail() { echo "FAIL  $VERSION  $1: $2"; FAILED=1; }

# spec NAME JS POLICY_JSON [EXTRA_JSON_FIELDS]
spec() {
  cat > "$D/$1.json" <<EOF
{"name":"$1","exec":"$NODE","args":["-e","$2","arg one"],"cwd":"$D",
 "env":{"FOO":"bar baz"},"node_ipc":true,
 "logs":{"out":"$D/$1.out","err":"$D/$1.err"},"policy":$3 ${4:-}}
EOF
}

# states FILE: lifecycle states from shim output, space separated.
states() { sed -n 's/.*"state":"\([a-z]*\)".*/\1/p' "$1" | tr '\n' ' ' | sed 's/ $//'; }

# run_bg NAME: start the shim in the background, output to $D/NAME.events.
run_bg() { "$VEXOR" shim --spec "$D/$1.json" > "$D/$1.events" 2>"$D/$1.stderr" & }

wait_for() { # wait_for FILE TEXT SECONDS
  i=0
  while ! grep -q "$2" "$1" 2>/dev/null; do
    i=$((i + 1)); [ "$i" -gt $(($3 * 10)) ] && return 1; sleep 0.1
  done
}

# 1. process.send('ready') over NODE_CHANNEL_FD gates `online`.
spec ready "setTimeout(function(){process.send('ready')},200);setTimeout(function(){process.exit(0)},400)" \
  '{"ready_timeout_ms":5000,"stop_exit_codes":[0]}'
"$VEXOR" shim --spec "$D/ready.json" > "$D/ready.events" 2>&1
got=$(states "$D/ready.events")
[ "$got" = "starting online stopped" ] && pass "ready via IPC" || fail "ready via IPC" "$got"

# 2. An open IPC channel doesn't keep a finished app alive.
spec natural "console.log('done')" '{"autorestart":false}'
run_bg natural; pid=$!
if wait_for "$D/natural.events" '"stopped"' 5; then pass "natural exit with IPC open"; else
  fail "natural exit with IPC open" "still running after 5s"; kill "$pid" 2>/dev/null; fi
wait "$pid" 2>/dev/null

# 3. SIGTERM to the shim stops the app gracefully.
spec term "process.on('SIGTERM',function(){console.log('term');process.exit(0)});console.log('up');setInterval(function(){},1000)" '{}'
run_bg term; pid=$!
wait_for "$D/term.out" up 5
kill -TERM "$pid"; wait "$pid"; code=$?
[ "$code" = 0 ] && grep -q term "$D/term.out" && pass "graceful SIGTERM" || fail "graceful SIGTERM" "exit=$code"

# 4. Crash, restart with backoff, crash-loop limit.
spec crash "console.log('run');process.exit(3)" '{"max_restarts":1}'
"$VEXOR" shim --spec "$D/crash.json" > "$D/crash.events" 2>&1; code=$?
runs=$(grep -c run "$D/crash.out")
[ "$code" = 1 ] && [ "$runs" = 2 ] && grep -q '"errored"' "$D/crash.events" \
  && pass "crash restart + crash-loop limit" || fail "crash restart + crash-loop limit" "exit=$code runs=$runs"

# 5. Arguments, environment, working directory, instance variable.
spec env "console.log([process.argv[1],process.env.FOO,process.cwd(),process.env.NODE_APP_INSTANCE].join('|'))" \
  '{"autorestart":false}' ',"instances":2'
"$VEXOR" shim --spec "$D/env.json" > "$D/env.events" 2>&1
want0="arg one|bar baz|$D|0"; want1="arg one|bar baz|$D|1"
if grep -qxF "$want0" "$D/env.out" && grep -qxF "$want1" "$D/env.out"; then
  pass "args, env, cwd, instances"; else fail "args, env, cwd, instances" "$(tr '\n' ';' < "$D/env.out")"; fi

# 6. Missing ready message fails readiness instead of reporting online.
spec noready "setInterval(function(){},1000)" '{"ready_timeout_ms":500,"max_restarts":0,"kill_timeout_ms":500}'
"$VEXOR" shim --spec "$D/noready.json" > "$D/noready.events" 2>&1
got=$(states "$D/noready.events")
[ "$got" = "starting stopping errored" ] && pass "readiness timeout" || fail "readiness timeout" "$got"

exit "$FAILED"
