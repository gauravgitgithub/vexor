#!/usr/bin/env node
// Launcher for the native `vexor` binary (docs/architecture.md §1, layer B).
//
// Finds the binary shipped in the platform package that npm installed
// alongside this one (vexord-<os>-<cpu>), runs it with the same arguments,
// forwards termination signals, and exits with its exit code or signal.
// The binary does all the work; this file only locates and starts it.
//
// Plain ES5 on purpose: it must run on old Node versions (see package.json
// "engines"). No dependencies, no install scripts, no network access.

'use strict';

var childProcess = require('child_process');
var fs = require('fs');
var path = require('path');

var SUPPORTED = {
  'darwin arm64': 'vexord-darwin-arm64',
  'darwin x64': 'vexord-darwin-x64',
  'linux arm64': 'vexord-linux-arm64',
  'linux x64': 'vexord-linux-x64'
};

function fail(message) {
  process.stderr.write('vexor: ' + message + '\n');
  process.exit(1);
}

function findBinary() {
  // Explicit override, e.g. for a binary installed with install.sh.
  if (process.env.VEXOR_BINARY) {
    return process.env.VEXOR_BINARY;
  }
  var key = process.platform + ' ' + process.arch;
  var pkg = SUPPORTED[key];
  if (!pkg) {
    fail(
      'no prebuilt binary for ' + key + '. Supported: ' +
        Object.keys(SUPPORTED).join(', ') +
        '. See https://github.com/gauravgitgithub/vexor#install for other options.'
    );
  }
  var manifest;
  try {
    manifest = require.resolve(pkg + '/package.json');
  } catch (e) {
    fail(
      'the package "' + pkg + '" is not installed, so the vexor binary is missing.\n' +
        '  It is an optional dependency of "vexord"; it is skipped when optional\n' +
        '  dependencies are disabled (--no-optional, --omit=optional) or when\n' +
        '  node_modules was copied from another platform.\n' +
        '  Fix: reinstall vexord on this machine with optional dependencies enabled,\n' +
        '  or install the binary directly and set VEXOR_BINARY to its path.'
    );
  }
  return path.join(path.dirname(manifest), 'bin', 'vexor');
}

function main() {
  var binary = findBinary();
  try {
    fs.accessSync(binary, fs.constants ? fs.constants.X_OK : 1);
  } catch (e) {
    fail('cannot execute ' + binary + ': ' + e.message);
  }

  var child = childProcess.spawn(binary, process.argv.slice(2), { stdio: 'inherit' });

  // Forward signals sent to this process (e.g. `kill <pid>`). A Ctrl-C in a
  // terminal reaches the binary directly as well; vexor handles a repeated
  // signal the same as a single one.
  var forwarded = ['SIGINT', 'SIGTERM', 'SIGHUP'];
  var handlers = {};
  forwarded.forEach(function (sig) {
    handlers[sig] = function () {
      try {
        child.kill(sig);
      } catch (e) {
        // Child already gone.
      }
    };
    process.on(sig, handlers[sig]);
  });

  child.on('error', function (err) {
    fail('cannot start ' + binary + ': ' + err.message);
  });

  child.on('exit', function (code, signal) {
    forwarded.forEach(function (sig) {
      process.removeListener(sig, handlers[sig]);
    });
    if (signal) {
      // Die the same way, so callers see the same status.
      process.kill(process.pid, signal);
      return;
    }
    process.exit(code === null ? 1 : code);
  });
}

main();
