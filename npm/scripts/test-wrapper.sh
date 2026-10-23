#!/bin/sh
# Checks the installed `vexord` npm launcher (plan Phase 3).
#
#   npm/scripts/test-wrapper.sh TGZ_DIR
#
# TGZ_DIR holds vexord-<version>.tgz and the platform package tarball for
# this machine (from `npm pack` of npm/scripts/stage.js output). Installs
# them into a temporary prefix, then checks the launcher. POSIX sh, so it
# runs in node:<v>-alpine too. Exits non-zero if any check fails.

set -u
TGZ=$(cd "$1" && pwd)
NODEV=$(node -v)
FAILED=0
T=$(mktemp -d)
pass() { echo "ok    $NODEV  $1"; }
fail() { echo "FAIL  $NODEV  $1: $2"; FAILED=1; }

wait_for() { # wait_for PATH SECONDS
  i=0
  while [ ! -e "$1" ]; do
    i=$((i + 1)); [ "$i" -gt $(($2 * 10)) ] && return 1; sleep 0.1
  done
}

wrapper=$(ls "$TGZ"/vexord-[0-9]*.tgz)
platform=$(ls "$TGZ"/vexord-*-*.tgz | grep -v "$wrapper" | head -n 1)

# Global install: launcher + platform package.
if npm install -g --prefix "$T/prefix" --no-audit --no-fund --loglevel=error "$platform" "$wrapper" >"$T/install.log" 2>&1; then
  pass "npm install -g ($(npm -v))"
else
  fail "npm install -g" "$(tail -n 5 "$T/install.log")"; exit 1
fi
V="$T/prefix/bin/vexor"
H="$T/home with space"

out=$("$V" --version 2>&1); [ "${out#vexor }" != "$out" ] && pass "--version" || fail "--version" "$out"

"$V" --home "$H" stop nope >/dev/null 2>&1; code=$?
[ "$code" = 1 ] && pass "exit code passthrough" || fail "exit code passthrough" "got $code"

"$V" --no-such-flag >/dev/null 2>&1; code=$?
[ "$code" = 2 ] && pass "usage error exit code" || fail "usage error exit code" "got $code"

# Arguments with spaces reach the binary intact (home path has a space).
"$V" --home "$H" list >/dev/null 2>&1 && [ -S "$H/daemon.sock" ] \
  && pass "arguments with spaces" || fail "arguments with spaces" "no socket in '$H'"
dpid=$(sed -n 's/.*"pid":\([0-9]*\).*"type":"daemon_started".*/\1/p' "$H/daemon.log" | tail -n 1)
[ -n "$dpid" ] && kill "$dpid" 2>/dev/null

# SIGTERM to the launcher reaches the binary (a foreground daemon exits 0).
H2="$T/h2"
"$V" --home "$H2" daemon >/dev/null 2>&1 & lpid=$!
if wait_for "$H2/daemon.sock" 10; then
  kill -TERM "$lpid"; wait "$lpid"; code=$?
  [ "$code" = 0 ] && [ ! -S "$H2/daemon.sock" ] && pass "SIGTERM forwarded" || fail "SIGTERM forwarded" "exit $code"
else
  fail "SIGTERM forwarded" "daemon did not start"; kill "$lpid" 2>/dev/null
fi

# A binary killed by a signal makes the launcher die by the same signal.
# Wait for the daemon's "started" log line, not its socket: the socket
# appears slightly earlier, and the pid comes from that line.
H3="$T/h3"
"$V" --home "$H3" daemon >/dev/null 2>&1 & lpid=$!
i=0; bpid=""
while [ -z "$bpid" ] && [ "$i" -lt 100 ]; do
  bpid=$(sed -n 's/.*"pid":\([0-9]*\).*"type":"daemon_started".*/\1/p' "$H3/daemon.log" 2>/dev/null | tail -n 1)
  i=$((i + 1)); [ -z "$bpid" ] && sleep 0.1
done
if [ -n "$bpid" ]; then
  kill -KILL "$bpid"; wait "$lpid"; code=$?
  [ "$code" = 137 ] && pass "signal death re-raised" || fail "signal death re-raised" "exit $code"
else
  fail "signal death re-raised" "daemon did not start"; kill -KILL "$lpid" 2>/dev/null
fi

# VEXOR_BINARY override.
out=$(VEXOR_BINARY=/bin/echo "$V" hello 2>&1)
[ "$out" = "hello" ] && pass "VEXOR_BINARY override" || fail "VEXOR_BINARY override" "$out"

# Launcher alone (platform package missing): actionable error, exit 1.
# Global installs ignore --omit=optional and fetch the platform package from
# the registry once it is published there, so remove it after installing.
npm install -g --prefix "$T/bare" --no-audit --no-fund --no-optional --omit=optional --loglevel=error "$wrapper" >/dev/null 2>&1
rm -rf "$T"/bare/lib/node_modules/vexord/node_modules/vexord-* "$T"/bare/lib/node_modules/vexord-*
out=$("$T/bare/bin/vexor" --version 2>&1); code=$?
case "$out" in
  *"is not installed"*) [ "$code" = 1 ] && pass "missing platform package" || fail "missing platform package" "exit $code" ;;
  *) fail "missing platform package" "$out" ;;
esac

exit "$FAILED"
