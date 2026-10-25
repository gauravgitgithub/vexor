#!/bin/sh
# Installs a published vexord with npm, pnpm and Yarn (1, and 4 with PnP
# and with node_modules), in a project and globally, and checks that each
# one runs: `--version`, then an app started, listed online and killed.
#
#   npm/scripts/check-package-managers.sh 0.12.0
#
# Needs Node with corepack. Retries the first install for a few minutes,
# since a just-published version takes a moment to reach the registry.
# Exits non-zero if any check fails.

set -u
V=${1:?usage: check-package-managers.sh VERSION}
FAILED=0
export COREPACK_ENABLE_DOWNLOAD_PROMPT=0
corepack enable >/dev/null 2>&1 || true
# Resolved: Yarn 1 writes dangling bin links under a symlinked path
# (macOS's /var is /private/var).
T=$(cd "$(mktemp -d)" && pwd -P)

# smoke LABEL COMMAND...: the command must be vexor, however it is reached.
smoke() {
  label=$1; shift
  h=$(mktemp -d "$T/h.XXXX")
  out=$("$@" --version 2>&1)
  case $out in
    "vexor $V") ;;
    *) echo "FAIL  $label: --version said: $out"; FAILED=1; return ;;
  esac
  "$@" --home "$h" start /bin/sleep --name s -- 60 >"$h/start.log" 2>&1
  state=$("$@" --home "$h" ls --json 2>/dev/null \
    | node -e 'let d="";process.stdin.on("data",c=>d+=c).on("end",()=>{try{console.log(JSON.parse(d)[0].snapshot[0].state)}catch(e){console.log("none")}})')
  "$@" --home "$h" kill >/dev/null 2>&1
  if [ "$state" = online ]; then
    echo "ok    $label"
  else
    echo "FAIL  $label: app state $state"; tail -n 3 "$h/start.log"; FAILED=1
  fi
}

# project LABEL INSTALL-COMMAND: install into a fresh project, then run the
# binary it linked in node_modules/.bin (Yarn PnP: through `yarn vexor`).
project() {
  label=$1; install=$2
  d=$(mktemp -d "$T/p.XXXX")
  echo '{"name":"p","version":"1.0.0","private":true}' >"$d/package.json"
  # Three tries: right after publishing, a mirror can still miss a package.
  n=0
  until (cd "$d" && sh -c "$install") >"$d/install.log" 2>&1; do
    n=$((n + 1))
    if [ "$n" -ge 3 ]; then
      echo "FAIL  $label: install"; tail -n 5 "$d/install.log"; FAILED=1; return
    fi
    sleep 20
  done
  if [ -x "$d/node_modules/.bin/vexor" ]; then
    smoke "$label" "$d/node_modules/.bin/vexor"
  else
    # Yarn Plug'n'Play has no node_modules; `yarn vexor` keeps `--` intact.
    (cd "$d" && smoke "$label" corepack yarn@4.5.0 vexor)
  fi
}

# The registry may not have the version yet: the launcher, and the binary
# package for this machine (published separately, seen a little later).
platform=$(node -p '"vexord-" + process.platform + "-" + process.arch')
i=0
until npm view "vexord@$V" version >/dev/null 2>&1 && npm view "$platform@$V" version >/dev/null 2>&1; do
  i=$((i + 1)); [ "$i" -gt 30 ] && { echo "FAIL  vexord@$V or $platform@$V is not on the registry"; exit 1; }
  sleep 10
done

project "npm project" "npm install --no-audit --no-fund vexord@$V"
project "pnpm 9 project" "corepack pnpm@9.15.0 add vexord@$V"
project "yarn 1 project" "corepack yarn@1.22.22 add vexord@$V"
project "yarn 4 project (pnp)" "corepack yarn@4.5.0 add vexord@$V"
project "yarn 4 project (node_modules)" "printf 'nodeLinker: node-modules\n' >.yarnrc.yml && corepack yarn@4.5.0 add vexord@$V"

if npm install -g --prefix "$T/npm-g" --no-audit --no-fund "vexord@$V" >"$T/g1.log" 2>&1; then
  smoke "npm global" "$T/npm-g/bin/vexor"
else
  echo "FAIL  npm global: install"; tail -n 5 "$T/g1.log"; FAILED=1
fi
if PNPM_HOME="$T/pnpm-g" PATH="$T/pnpm-g:$PATH" corepack pnpm@9.15.0 add -g "vexord@$V" >"$T/g2.log" 2>&1; then
  smoke "pnpm global" "$T/pnpm-g/vexor"
else
  echo "FAIL  pnpm global: install"; tail -n 5 "$T/g2.log"; FAILED=1
fi
# Yarn 1 doesn't create the prefix's bin directory itself.
mkdir -p "$T/yarn-g/bin"
if corepack yarn@1.22.22 global add "vexord@$V" --prefix "$T/yarn-g" >"$T/g3.log" 2>&1; then
  smoke "yarn 1 global" "$T/yarn-g/bin/vexor"
else
  echo "FAIL  yarn 1 global: install"; tail -n 5 "$T/g3.log"; FAILED=1
fi

exit $FAILED
