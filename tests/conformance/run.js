#!/usr/bin/env node
// PM2 conformance harness (plan step 4.6).
//
// Runs every scenario in scenarios.json under PM2 and under Vexor, each in
// an isolated home (PM2_HOME / --home), and compares what the probe app
// experienced (runs, argv, cwd, environment, instance numbers, signals) and
// what each tool reported (status, restart count).
//
// Meant for a throwaway container with `pm2` and `vexor` on PATH:
//   node run.js [--out report.md] [--json results.json]
// It never touches an existing PM2 installation's home.

'use strict';

const { execFileSync } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

const here = __dirname;
const scenarios = JSON.parse(fs.readFileSync(path.join(here, 'scenarios.json'), 'utf8'));
const arg = (name) => {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
};
const sleep = (s) => new Promise((r) => setTimeout(r, s * 1000));
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'conformance-'));

function sh(cmd, args, env, { allowFail = false, cwd } = {}) {
  try {
    return execFileSync(cmd, args, {
      cwd,
      env: { ...process.env, ...env },
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
      timeout: 60_000,
    });
  } catch (e) {
    if (allowFail) return (e.stdout || '') + (e.stderr || '');
    throw new Error(`${cmd} ${args.join(' ')} failed: ${e.stderr || e.message}`);
  }
}

/** Write the scenario's app and ecosystem file into `dir`. */
function prepare(dir, s) {
  fs.mkdirSync(dir, { recursive: true });
  const obs = path.join(dir, 'obs.jsonl');
  const a = s.app;
  const probeDir = a.probe_in_sub ? path.join(dir, 'sub') : dir;
  fs.mkdirSync(probeDir, { recursive: true });
  if (a.cwd) fs.mkdirSync(path.join(dir, a.cwd), { recursive: true });
  fs.copyFileSync(path.join(here, 'probe.js'), path.join(probeDir, 'probe.js'));
  const app = {
    name: s.id,
    script: a.script || 'probe.js',
    env: { OBS: obs, BEHAVIOR: a.behavior || 'run', ...(a.env || {}) },
  };
  for (const k of [
    'cwd', 'args', 'instances', 'exec_mode', 'env_production', 'max_restarts',
    'min_uptime', 'restart_delay', 'stop_exit_codes', 'autorestart', 'kill_timeout',
    'wait_ready', 'listen_timeout',
  ]) {
    if (a[k] !== undefined) app[k] = a[k];
  }
  fs.writeFileSync(
    path.join(dir, 'ecosystem.config.js'),
    `module.exports = ${JSON.stringify({ apps: [app] }, null, 2)};\n`,
  );
  return obs;
}

function observations(obs, dir) {
  const lines = fs.existsSync(obs)
    ? fs.readFileSync(obs, 'utf8').trim().split('\n').filter(Boolean).map((l) => JSON.parse(l))
    : [];
  const starts = lines.filter((l) => l.event === 'start');
  const first = starts[0] || {};
  return {
    runs: starts.length,
    argv: first.argv ?? null,
    cwd: first.cwd ? path.relative(dir, first.cwd) || '.' : null,
    mode: first.mode ?? null,
    a: first.a ?? null,
    insts: [...new Set(starts.map((x) => x.inst))].sort(),
    signal: (lines.find((l) => l.event === 'signal') || {}).signal ?? null,
  };
}

async function runPm2(s, dir) {
  const env = { PM2_HOME: path.join(dir, '.pm2') };
  const args = ['start', 'ecosystem.config.js', ...(s.env_name ? ['--env', s.env_name] : [])];
  const startOut = sh('pm2', args, env, { allowFail: true, cwd: dir });
  await sleep(s.wait);
  const status = () => {
    const list = JSON.parse(sh('pm2', ['jlist'], env, { allowFail: true }) || '[]');
    return {
      statuses: list.map((p) => p.pm2_env.status).sort(),
      restarts: list.map((p) => p.pm2_env.restart_time).sort(),
    };
  };
  let st = status();
  if (s.stop) {
    sh('pm2', ['stop', s.id], env, { allowFail: true });
    await sleep((s.app.kill_timeout || 1600) / 1000 + 1);
    st = status();
  }
  sh('pm2', ['kill'], env, { allowFail: true });
  return { ...st, note: /error/i.test(startOut) ? startOut.split('\n').find((l) => /error/i.test(l)) : null };
}

async function runVexor(s, dir) {
  const home = ['--home', path.join(dir, '.vexor')];
  const args = [
    ...home, 'start', 'ecosystem.config.js', '--allow-unsupported',
    ...(s.env_name ? ['--env', s.env_name] : []),
  ];
  const startOut = sh('vexor', args, {}, { allowFail: true, cwd: dir });
  await sleep(s.wait);
  const status = () => {
    const apps = JSON.parse(sh('vexor', [...home, 'list', '--json'], {}, { allowFail: true }) || '[]');
    const snap = apps.flatMap((a) => a.snapshot || []);
    return {
      statuses: snap.map((x) => x.state).sort(),
      restarts: snap.map((x) => x.restarts).sort(),
    };
  };
  let st = status();
  if (s.stop) {
    sh('vexor', [...home, 'stop', s.id], {}, { allowFail: true });
    st = status();
    st.statuses = ['stopped'];
  }
  sh('vexor', [...home, 'delete', 'all'], {}, { allowFail: true });
  const log = path.join(dir, '.vexor', 'daemon.log');
  if (fs.existsSync(log)) {
    const pids = [...fs.readFileSync(log, 'utf8').matchAll(/"pid":(\d+),"ts_ms":\d+,"type":"daemon_started"/g)];
    for (const m of pids) {
      try { process.kill(Number(m[1])); } catch { /* gone */ }
    }
  }
  return { ...st, note: /error|warning/i.test(startOut) ? startOut.trim().split('\n').filter((l) => /error/i.test(l)).join(' ') || null : null };
}

const FIELDS = ['runs', 'statuses', 'restarts', 'argv', 'cwd', 'mode', 'a', 'insts', 'signal'];

async function main() {
  const pm2Version = sh('pm2', ['--version'], { PM2_HOME: path.join(root, '.pm2-version') }, { allowFail: true }).trim().split('\n').pop();
  const vexorVersion = sh('vexor', ['--version'], {}).trim();
  const results = [];
  for (const s of scenarios) {
    const out = {};
    for (const [tool, runner] of [['pm2', runPm2], ['vexor', runVexor]]) {
      const dir = path.join(root, tool, s.id);
      const obs = prepare(dir, s);
      const status = await runner(s, dir);
      out[tool] = { ...observations(obs, dir), ...status };
    }
    // A scenario where the app never ran under either tool tests nothing:
    // it means the harness is broken, not that the tools agree.
    if (out.pm2.runs === 0 && out.vexor.runs === 0) {
      throw new Error(`${s.id}: the app never ran under either tool (pm2: ${out.pm2.note}; vexor: ${out.vexor.note})`);
    }
    const diffs = FIELDS.filter((f) => JSON.stringify(out.pm2[f]) !== JSON.stringify(out.vexor[f]));
    results.push({ id: s.id, question: s.question, pm2: out.pm2, vexor: out.vexor, diffs });
    process.stderr.write(`${s.id}: ${diffs.length ? 'differs in ' + diffs.join(', ') : 'same'}\n`);
  }

  const fmt = (v) => (v === null || v === undefined ? '–' : '`' + JSON.stringify(v) + '`');
  let md = `# PM2 conformance results\n\nGenerated by \`tests/conformance/run.js\`: ${pm2Version ? 'PM2 ' + pm2Version : 'PM2'} vs ${vexorVersion}, Node ${process.version}, ${os.platform()}/${os.arch()}, ${new Date().toISOString().slice(0, 10)}.\n\n`;
  md += `Each scenario runs the same ecosystem file under both tools. Rows show what the probe app saw and what each tool reported; **bold** rows differ.\n\n`;
  for (const r of results) {
    md += `## ${r.id}\n\n${r.question}\n\n| | PM2 | Vexor |\n|---|---|---|\n`;
    for (const f of FIELDS) {
      const row = `| ${f} | ${fmt(r.pm2[f])} | ${fmt(r.vexor[f])} |\n`;
      md += r.diffs.includes(f) ? row.replace(/^\| (\w+) /, '| **$1** ') : row;
    }
    for (const t of ['pm2', 'vexor']) {
      if (r[t].note) md += `\n${t} said: \`${r[t].note.slice(0, 300)}\`\n`;
    }
    md += '\n';
  }
  const outPath = arg('--out');
  if (outPath) fs.writeFileSync(outPath, md);
  else process.stdout.write(md);
  const jsonPath = arg('--json');
  if (jsonPath) fs.writeFileSync(jsonPath, JSON.stringify({ pm2Version, vexorVersion, results }, null, 2));
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
