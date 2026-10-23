#!/usr/bin/env node
// Vexor vs PM2 benchmarks (docs/benchmarks.md). Runs inside a throwaway
// Linux container with `pm2` and `vexor` on PATH:
//
//   node benchmarks/run.js --out results.json
//
// Every number is measured here; nothing is estimated. Each tool runs alone
// (the other's daemon is stopped) on the same Node binary and apps.

'use strict';

const { execFileSync, spawn } = require('child_process');
const fs = require('fs');
const http = require('http');
const os = require('os');
const path = require('path');

const arg = (n, d) => {
  const i = process.argv.indexOf(n);
  return i >= 0 ? process.argv[i + 1] : d;
};
const TRIALS = Number(arg('--trials', 15));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const now = () => Number(process.hrtime.bigint() / 1000000n);
const work = fs.mkdtempSync(path.join(os.tmpdir(), 'bench-'));
const PM2_HOME = path.join(work, 'pm2');
const VX_HOME = path.join(work, 'vx');

function sh(cmd, args, opts = {}) {
  return execFileSync(cmd, args, {
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, PM2_HOME },
    timeout: 120000,
    ...opts,
  });
}
const pm2 = (...a) => sh('pm2', a);
const vx = (...a) => sh('vexor', ['--home', VX_HOME, ...a]);
const ox = (...a) => sh('oxmgr', a);
const fv = (...a) => sh('forever', a, { env: { ...process.env, ...(a.env || {}) } });

function stats(xs) {
  const s = [...xs].sort((a, b) => a - b);
  const q = (p) => s[Math.min(s.length - 1, Math.floor(p * s.length))];
  return { median: q(0.5), p95: q(0.95), min: s[0], max: s[s.length - 1], n: s.length };
}

/**
 * RSS and PSS (bytes) and CPU ticks (utime+stime) of a pid, from /proc.
 * RSS counts shared pages (e.g. one binary's code) in every process that
 * maps them; PSS splits them between those processes, so a sum of PSS is
 * the memory the processes really use together.
 */
function proc(pid) {
  try {
    const status = fs.readFileSync(`/proc/${pid}/status`, 'utf8');
    const rss = Number(/VmRSS:\s+(\d+)/.exec(status)[1]) * 1024;
    let pss = rss;
    try {
      const rollup = fs.readFileSync(`/proc/${pid}/smaps_rollup`, 'utf8');
      pss = Number(/^Pss:\s+(\d+)/m.exec(rollup)[1]) * 1024;
    } catch (e) {}
    const stat = fs.readFileSync(`/proc/${pid}/stat`, 'utf8');
    const f = stat.slice(stat.lastIndexOf(')') + 2).split(' ');
    return { rss, pss, ticks: Number(f[11]) + Number(f[12]) };
  } catch (e) {
    return { rss: 0, pss: 0, ticks: 0 };
  }
}

function pidsMatching(re) {
  return fs
    .readdirSync('/proc')
    .filter((d) => /^\d+$/.test(d))
    .filter((d) => {
      try {
        return re.test(fs.readFileSync(`/proc/${d}/cmdline`, 'utf8').replace(/\0/g, ' '));
      } catch (e) {
        return false;
      }
    })
    .map(Number);
}

/** App processes only (forever's monitors carry the script path too). */
const appPids = (re) => {
  const monitors = new Set(pidsMatching(/forever\/bin\/monitor/));
  return pidsMatching(re).filter((p) => !monitors.has(p));
};

const pm2Supervisor = () => pidsMatching(/^PM2 v/);
const vexorSupervisor = () => pidsMatching(/vexor --home \S+ (daemon|shim)/);

/**
 * One adapter per tool. apps: [{ name, script, env, instances }].
 * A tool without a capability leaves it undefined; its result is null.
 */
const TOOLS = {
  pm2: {
    start(apps) {
      const eco = path.join(work, `pm2-${nextFile++}.config.js`);
      const list = apps.map((a) => Object.assign({ name: a.name, script: a.script, env: a.env || {} },
        a.instances ? { exec_mode: 'cluster', instances: a.instances, wait_ready: true, listen_timeout: 5000 } : {}));
      fs.writeFileSync(eco, 'module.exports = ' + JSON.stringify({ apps: list }) + ';\n');
      pm2('start', eco);
    },
    list: () => pm2('ls'),
    online: () => JSON.parse(pm2('jlist')).filter((p) => p.pm2_env.status === 'online').length,
    supervisor: pm2Supervisor,
    daemon: () => pm2Supervisor()[0],
    reload: (name) => pm2('reload', name),
    stop() { try { pm2('kill'); } catch (e) {} },
  },
  vexor: {
    start(apps) {
      const eco = path.join(work, `vx-${nextFile++}.config.js`);
      const list = apps.map((a) => Object.assign({ name: a.name, script: a.script, env: a.env || {} },
        a.instances ? { exec_mode: 'cluster', instances: a.instances, wait_ready: true, listen_timeout: 5000 } : {}));
      fs.writeFileSync(eco, 'module.exports = ' + JSON.stringify({ apps: list }) + ';\n');
      vx('start', eco);
    },
    list: () => vx('ls'),
    online: () => JSON.parse(vx('ls', '--json')).flatMap((a) => a.snapshot || []).filter((s) => s.state === 'online').length,
    supervisor: vexorSupervisor,
    daemon: () => pidsMatching(/vexor --home \S+ daemon/)[0],
    reload: (name) => vx('reload', name),
    stop() {
      try { vx('delete', 'all'); } catch (e) {}
      for (const p of vexorSupervisor()) { try { process.kill(p, 'SIGKILL'); } catch (e) {} }
    },
  },
  oxmgr: {
    // Its PM2 ecosystem import doesn't run .js scripts with Node (found
    // while writing this), so apps are started one by one with `node`.
    start(apps) {
      for (const a of apps) {
        const args = ['start', `node ${a.script}`, '--name', a.name, '--restart', 'always'];
        for (const [k, v] of Object.entries(a.env || {})) args.push('--env', `${k}=${v}`);
        if (a.instances) args.push('--cluster', '--cluster-instances', String(a.instances));
        ox(...args);
      }
    },
    list: () => ox('list'),
    online: () => JSON.parse(ox('list', '--json'))
      .filter((p) => p.status === 'running')
      .reduce((n, p) => n + (p.cluster_mode ? p.cluster_instances || 1 : 1), 0),
    supervisor: () => pidsMatching(/oxmgr daemon run/),
    daemon: () => pidsMatching(/oxmgr daemon run/)[0],
    reload: (name) => ox('reload', name),
    stop() {
      try { ox('daemon', 'stop'); } catch (e) {}
      for (const p of pidsMatching(/oxmgr daemon run/)) { try { process.kill(p, 'SIGKILL'); } catch (e) {} }
      fs.rmSync(path.join(os.homedir(), '.local/share/oxmgr'), { recursive: true, force: true });
    },
  },
  forever: {
    // One monitor process per app; no cluster mode, no reload, no daemon.
    start(apps) {
      for (const a of apps) {
        sh('forever', ['start', '-a', '--uid', a.name, a.script], { env: { ...process.env, ...(a.env || {}) } });
      }
    },
    list: () => sh('forever', ['list']),
    online: () => appPids(/apps\/(idle|http)\.js/).length,
    supervisor: () => pidsMatching(/forever\/bin\/monitor/),
    stop() {
      try { sh('forever', ['stopall']); } catch (e) {}
      for (const p of pidsMatching(/forever\/bin\/monitor/)) { try { process.kill(p, 'SIGKILL'); } catch (e) {} }
    },
  },
};
let nextFile = 0;

// ------------------------------------------------------------------ apps

const APPS = path.join(work, 'apps');
fs.mkdirSync(APPS, { recursive: true });
fs.writeFileSync(path.join(APPS, 'idle.js'), 'setInterval(function () {}, 1000);\n');
fs.writeFileSync(
  path.join(APPS, 'http.js'),
  "require('http').createServer(function (q, s) { s.end('ok'); }).listen(Number(process.env.PORT), function () { if (process.send) process.send('ready'); });\n"
);


const online = (tool) => TOOLS[tool].online();

async function waitOnline(tool, n, timeoutMs = 60000) {
  const end = now() + timeoutMs;
  while (now() < end) {
    try { if (online(tool) >= n) return true; } catch (e) {}
    await sleep(20);
  }
  return false;
}

function stopAll() {
  for (const t of Object.values(TOOLS)) t.stop();
  for (const p of pidsMatching(/apps\/(idle|http)\.js/)) {
    try { process.kill(p, 'SIGKILL'); } catch (e) {}
  }
}

const idleApps = (n) => Array.from({ length: n }, (_, i) => ({ name: 'idle' + i, script: path.join(APPS, 'idle.js') }));

// ------------------------------------------------------------ benchmarks

/** B1: CLI response time for the list command, daemon running. */
async function cliLatency(tool) {
  TOOLS[tool].start(idleApps(5));
  await waitOnline(tool, 5);
  const times = [];
  for (let i = 0; i < TRIALS + 2; i++) {
    const t = now();
    TOOLS[tool].list();
    if (i >= 2) times.push(now() - t);
  }
  stopAll();
  return stats(times);
}

/** B5: time from `start <ecosystem with N apps>` to all online. */
async function startTime(tool, n) {
  const times = [];
  for (let i = 0; i < 5; i++) {
    const t = now();
    TOOLS[tool].start(idleApps(n));
    await waitOnline(tool, n);
    times.push(now() - t);
    stopAll();
  }
  return stats(times);
}

/** B3: supervisor RSS and CPU with N idle apps, over 30 s. */
async function idle(tool, n) {
  TOOLS[tool].start(idleApps(n));
  await waitOnline(tool, n);
  await sleep(3000);
  const sup = TOOLS[tool].supervisor;
  const apps = () => appPids(/idle\.js/);
  const sum = (pids) => pids.map(proc).reduce((a, b) => ({ rss: a.rss + b.rss, pss: a.pss + b.pss, ticks: a.ticks + b.ticks }), { rss: 0, pss: 0, ticks: 0 });
  const before = sum(sup());
  const t0 = now();
  await sleep(30000);
  const after = sum(sup());
  const secs = (now() - t0) / 1000;
  const hz = 100;
  const result = {
    supervisor_rss: after.rss,
    supervisor_pss: after.pss,
    supervisor_processes: sup().length,
    supervisor_cpu_pct: ((after.ticks - before.ticks) / hz / secs) * 100,
    app_rss: sum(apps()).rss,
  };
  stopAll();
  return result;
}

/** B7: kill -9 an app, time until a new process is online. */
async function crashRestart(tool) {
  TOOLS[tool].start(idleApps(1));
  await waitOnline(tool, 1);
  const times = [];
  for (let i = 0; i < 8; i++) {
    await sleep(1500); // past min_uptime, so each crash is "stable"
    const [pid] = appPids(/idle\.js/);
    const t = now();
    process.kill(pid, 'SIGKILL');
    const giveUp = now() + 30000;
    for (;;) {
      const fresh = appPids(/idle\.js/).filter((p) => p !== pid);
      if (fresh.length && online(tool) >= 1) break;
      if (now() > giveUp) throw new Error('not restarted within 30 s');
      await sleep(5);
    }
    times.push(now() - t);
  }
  stopAll();
  return stats(times);
}

/** HTTP load: requests every 5 ms; counts failures. */
function load(port) {
  let ok = 0;
  let failed = 0;
  let stop = false;
  const loop = async () => {
    while (!stop) {
      await new Promise((resolve) => {
        const req = http.get({ host: '127.0.0.1', port, path: '/', agent: false, timeout: 2000 }, (res) => {
          res.resume();
          res.on('end', () => { if (res.statusCode === 200) ok++; else failed++; resolve(); });
        });
        req.on('error', () => { failed++; resolve(); });
        req.on('timeout', () => { req.destroy(); });
      });
      await sleep(5);
    }
  };
  const runners = [loop(), loop()];
  return async () => { stop = true; await Promise.all(runners); return { ok, failed }; };
}

let nextPort = 21000;

/** B13: failed requests during a cluster reload (2 instances). */
async function reload(tool) {
  if (!TOOLS[tool].reload) return null;
  const port = nextPort++;
  TOOLS[tool].start([{ name: 'web', script: path.join(APPS, 'http.js'), env: { PORT: String(port) }, instances: 2 }]);
  await waitOnline(tool, 2);
  await sleep(1500);
  const done = load(port);
  for (let i = 0; i < 5; i++) {
    TOOLS[tool].reload('web');
    await sleep(1000);
  }
  const r = await done();
  stopAll();
  return Object.assign({ reloads: 5 }, r);
}

/** B12: kill -9 the supervisor daemon; is the app still served and managed? */
async function daemonCrash(tool) {
  if (!TOOLS[tool].daemon) return null;
  const port = nextPort++;
  TOOLS[tool].start([{ name: 'web', script: path.join(APPS, 'http.js'), env: { PORT: String(port) } }]);
  await waitOnline(tool, 1);
  await sleep(500);
  const done = load(port);
  await sleep(500);
  const daemon = TOOLS[tool].daemon();
  process.kill(daemon, 'SIGKILL');
  await sleep(3000);
  // Next CLI command (starts a new daemon if needed): is the app listed?
  let listed = 0;
  try { listed = online(tool); } catch (e) {}
  const r = await done();
  stopAll();
  return Object.assign({ still_managed: listed >= 1 }, r);
}

function installSize(dir) {
  const out = sh('du', ['-sk', dir]);
  return Number(out.split(/\s/)[0]) * 1024;
}

async function main() {
  stopAll();
  const node = process.version;
  const pm2Version = pm2('-v').trim().split('\n').pop();
  const vexorVersion = vx('--version').trim();
  const versions = {
    pm2: pm2Version,
    vexor: vexorVersion.replace('vexor ', ''),
    oxmgr: ox('--version').trim().replace('oxmgr ', ''),
    forever: sh('forever', ['--version']).trim().split('\n').pop().replace(/^v/, ''),
  };
  const results = { meta: {}, versions, pm2: {}, vexor: {}, oxmgr: {}, forever: {} };
  results.meta = {
    date: new Date().toISOString().slice(0, 10),
    node,
    pm2: pm2Version,
    vexor: vexorVersion,
    cpu: process.env.HOST_CPU || os.cpus()[0].model,
    cores: os.cpus().length,
    mem_gb: Math.round(os.totalmem() / 1e9),
    platform: `${os.platform()}/${os.arch()} ${os.release()}`,
    container: fs.existsSync('/.dockerenv'),
    trials: TRIALS,
  };
  // A failure in one measurement is recorded, never fatal for the run.
  const measure = async (tool, label, fn) => {
    process.stderr.write(`${tool}: ${label}\n`);
    try {
      return await fn();
    } catch (e) {
      stopAll();
      return { error: String(e.message || e).split('\n')[0].slice(0, 200) };
    }
  };
  for (const tool of ['pm2', 'vexor', 'oxmgr', 'forever']) {
    const r = results[tool];
    r.cli_ms = await measure(tool, 'cli', () => cliLatency(tool));
    r.start10_ms = await measure(tool, 'start', () => startTime(tool, 10));
    for (const n of [1, 10, 50]) {
      r[`idle_${n}`] = await measure(tool, `idle ${n}`, () => idle(tool, n));
    }
    r.crash_restart_ms = await measure(tool, 'crash restart', () => crashRestart(tool));
    r.reload = await measure(tool, 'reload', () => reload(tool));
    r.daemon_crash = await measure(tool, 'daemon crash', () => daemonCrash(tool));
  }
  const root = sh('npm', ['root', '-g']).trim();
  results.pm2.install_bytes = installSize(path.join(root, 'pm2'));
  results.vexor.install_bytes = installSize(path.join(root, 'vexord'));
  results.oxmgr.install_bytes = installSize(path.join(root, 'oxmgr'));
  results.forever.install_bytes = installSize(path.join(root, 'forever'));
  const out = arg('--out');
  const text = JSON.stringify(results, null, 2);
  if (out) fs.writeFileSync(out, text + '\n');
  process.stdout.write(text + '\n');
}

main().catch((e) => {
  console.error(e);
  stopAll();
  process.exit(1);
});
