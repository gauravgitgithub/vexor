// Probe app for PM2 conformance scenarios. Records how it was started, then
// behaves as BEHAVIOR says. Observations go to $OBS (one JSON line each).
'use strict';
var fs = require('fs');
var obs = process.env.OBS;
function note(o) {
  o.pid = process.pid;
  o.t = Date.now();
  fs.appendFileSync(obs, JSON.stringify(o) + '\n');
}
note({
  event: 'start',
  argv: process.argv.slice(2),
  cwd: process.cwd(),
  inst: process.env.NODE_APP_INSTANCE === undefined ? null : process.env.NODE_APP_INSTANCE,
  mode: process.env.MODE === undefined ? null : process.env.MODE,
  a: process.env.A === undefined ? null : process.env.A,
});
var behavior = process.env.BEHAVIOR || 'run';
['SIGINT', 'SIGTERM'].forEach(function (sig) {
  process.on(sig, function () {
    note({ event: 'signal', signal: sig });
    process.exit(0);
  });
});
if (behavior === 'crash') process.exit(1);
if (behavior === 'exit0') process.exit(0);
if (behavior === 'ready' && process.send) process.send('ready');
setInterval(function () {}, 1000);
