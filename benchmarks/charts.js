#!/usr/bin/env node
// Turn benchmarks/run.js results into SVG bar charts and a Markdown table.
//
//   node benchmarks/charts.js results.json docs/benchmarks
//
// Charts use mid-tone colours on a transparent background, so they read on
// GitHub's light and dark themes alike. A tool without a result for a
// measure (not supported, or it failed) gets no bar and "n/a" in the table.

'use strict';

const fs = require('fs');
const path = require('path');

const [, , input, outDir] = process.argv;
const r = JSON.parse(fs.readFileSync(input, 'utf8'));
fs.mkdirSync(outDir, { recursive: true });

const TOOLS = ['vexor', 'pm2', 'oxmgr', 'forever'].filter((t) => r[t]);
const NAMES = { vexor: 'Vexor', pm2: 'PM2', oxmgr: 'OxMgr', forever: 'forever' };
const COLORS = { vexor: '#14b8a6', pm2: '#94a3b8', oxmgr: '#f59e0b', forever: '#a78bfa' };
const TEXT = '#8b949e';
const versions = r.versions || { pm2: r.meta.pm2, vexor: r.meta.vexor };
const label = (t) => `${NAMES[t]} ${versions[t] || ''}`.trim();
const MiB = (b) => b / 1048576;
const esc = (s) => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;');
const one = (x) => (x < 10 ? x.toFixed(1) : Math.round(x).toString());
const ok = (v) => v !== null && v !== undefined && !(typeof v === 'object' && v.error);

/** groups: [{ label, get: (tool) => number|undefined }] */
function chart(file, title, subtitle, unit, groups) {
  const W = 780;
  const left = 120;
  const right = 110;
  const barH = 16;
  const gap = 5;
  const groupGap = 20;
  const top = 66;
  const vals = groups.flatMap((g) => TOOLS.map((t) => g.get(t)).filter((v) => typeof v === 'number'));
  const max = Math.max(...vals, 0) || 1;
  const scale = (v) => (v / max) * (W - left - right);
  let y = top;
  let body = '';
  for (const g of groups) {
    const shown = TOOLS.filter((t) => typeof g.get(t) === 'number');
    const midY = y + (shown.length * (barH + gap)) / 2;
    body += `<text x="${left - 12}" y="${midY}" text-anchor="end" class="l">${esc(g.label)}</text>`;
    for (const t of TOOLS) {
      const v = g.get(t);
      if (typeof v !== 'number') continue;
      const w = Math.max(2, scale(v));
      body += `<rect x="${left}" y="${y}" width="${w.toFixed(1)}" height="${barH}" rx="3" fill="${COLORS[t]}"/>`;
      body += `<text x="${left + w + 6}" y="${y + barH - 4}" class="v">${esc(one(v))}${unit} · ${esc(NAMES[t])}</text>`;
      y += barH + gap;
    }
    y += groupGap - gap;
  }
  let lx = left;
  let legend = '';
  const ly = y + 6;
  for (const t of TOOLS) {
    legend += `<rect x="${lx}" y="${ly}" width="12" height="12" rx="2" fill="${COLORS[t]}"/><text x="${lx + 17}" y="${ly + 10}" class="v">${esc(label(t))}</text>`;
    lx += 30 + label(t).length * 7;
  }
  const H = ly + 26;
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${W} ${H}" width="${W}" height="${H}" font-family="-apple-system,Segoe UI,Helvetica,Arial,sans-serif">
<style>.t{font-size:17px;font-weight:600;fill:${TEXT}}.s{font-size:12px;fill:${TEXT}}.l{font-size:13px;fill:${TEXT}}.v{font-size:12px;fill:${TEXT}}</style>
<text x="20" y="28" class="t">${esc(title)}</text>
<text x="20" y="48" class="s">${esc(subtitle)}</text>
${body}${legend}
</svg>
`;
  fs.writeFileSync(path.join(outDir, file), svg);
}

const med = (key) => (t) => (ok(r[t][key]) ? r[t][key].median : undefined);
const idle = (n, f) => (t) => (ok(r[t][`idle_${n}`]) ? f(r[t][`idle_${n}`]) : undefined);

chart('cli-latency.svg', 'CLI response time: list apps',
  `Median of ${r.meta.trials} runs, daemon running, 5 apps, installed from npm. Lower is better.`, ' ms',
  [{ label: 'list', get: med('cli_ms') }]);
const memOf = (x) => MiB(x.supervisor_pss || x.supervisor_rss);
chart('supervisor-memory.svg', 'Supervisor memory (PSS)',
  'Memory the process manager uses besides your apps, shared pages split between processes. Lower is better.', ' MiB',
  [1, 10, 50].map((n) => ({ label: `${n} app${n > 1 ? 's' : ''}`, get: idle(n, memOf) })));
chart('supervisor-cpu.svg', 'Supervisor CPU while apps are idle',
  'Average over 30 s, % of one core. Lower is better.', '%',
  [1, 10, 50].map((n) => ({ label: `${n} app${n > 1 ? 's' : ''}`, get: idle(n, (x) => x.supervisor_cpu_pct) })));
chart('start-time.svg', 'Start 10 apps until all are online',
  'Median of 5 runs. Lower is better.', ' ms', [{ label: '10 apps', get: med('start10_ms') }]);
chart('crash-restart.svg', 'Crash to back online',
  'kill -9 of an app, median of 8. Lower is better.', ' ms', [{ label: 'restart', get: med('crash_restart_ms') }]);
chart('install-size.svg', 'Installed size', 'npm install -g, on disk. Lower is better.', ' MiB',
  [{ label: 'install', get: (t) => (r[t].install_bytes ? MiB(r[t].install_bytes) : undefined) }]);

// Markdown table: one column per tool, best value in bold.
function row(name, get, fmt, lower = true) {
  const vals = TOOLS.map((t) => get(t));
  const nums = vals.filter((v) => typeof v === 'number');
  const best = nums.length ? (lower ? Math.min(...nums) : Math.max(...nums)) : null;
  const cells = vals.map((v) => (typeof v !== 'number' ? 'n/a' : v === best && nums.length > 1 ? `**${fmt(v)}**` : fmt(v)));
  return `| ${name} | ${cells.join(' | ')} |`;
}
const failed = (key) => (t) => (ok(r[t][key]) ? r[t][key].failed : undefined);
const total = (key) => (t) => (ok(r[t][key]) ? r[t][key].ok + r[t][key].failed : 0);
const m = r.meta;
const lines = [
  `Measured ${m.date} on ${m.cpu} (${m.cores} cores, ${m.mem_gb} GB), ${m.platform}${m.container ? ', in a container' : ''}, Node ${m.node}. Each tool runs alone, installed from npm, on the same apps. **Bold** = best; n/a = not supported or failed (see notes).`,
  '',
  `| Measure | ${TOOLS.map(label).join(' | ')} |`,
  `|---|${TOOLS.map(() => '---').join('|')}|`,
  row('List apps (CLI), median', med('cli_ms'), (v) => `${one(v)} ms`),
  row('Start 10 apps until online, median', med('start10_ms'), (v) => `${one(v)} ms`),
  ...[1, 10, 50].map((n) => row(`Supervisor memory (PSS), ${n} app${n > 1 ? 's' : ''}`, idle(n, memOf), (v) => `${one(v)} MiB`)),
  ...[1, 10, 50].map((n) => row(`Supervisor memory (RSS), ${n} app${n > 1 ? 's' : ''}`, idle(n, (x) => MiB(x.supervisor_rss)), (v) => `${one(v)} MiB`)),
  ...[1, 10, 50].map((n) => row(`Supervisor CPU while idle, ${n} app${n > 1 ? 's' : ''}`, idle(n, (x) => x.supervisor_cpu_pct), (v) => `${v.toFixed(2)}%`)),
  row('Crash to back online, median', med('crash_restart_ms'), (v) => `${one(v)} ms`),
  row('Failed requests in 5 cluster reloads', failed('reload'), (v) => `${v}`),
  row('Supervisor daemon killed: failed requests', failed('daemon_crash'), (v) => `${v}`),
  `| Supervisor daemon killed: app still managed | ${TOOLS.map((t) => (ok(r[t].daemon_crash) ? (r[t].daemon_crash.still_managed ? 'yes' : '**no**') : 'n/a')).join(' | ')} |`,
  row('Installed size (npm)', (t) => (r[t].install_bytes ? MiB(r[t].install_bytes) : undefined), (v) => `${one(v)} MiB`),
  '',
  `Requests sent: reloads ${TOOLS.map((t) => `${NAMES[t]} ${total('reload')(t)}`).join(', ')}; daemon kill ${TOOLS.map((t) => `${NAMES[t]} ${total('daemon_crash')(t)}`).join(', ')}.`,
];
const notes = [];
for (const t of TOOLS) {
  for (const [k, v] of Object.entries(r[t])) {
    if (v && typeof v === 'object' && v.error) notes.push(`- ${NAMES[t]}, ${k}: ${v.error}`);
  }
}
if (notes.length) lines.push('', 'Notes:', ...notes);
fs.writeFileSync(path.join(outDir, 'results.md'), lines.join('\n') + '\n');
process.stdout.write(lines.join('\n') + '\n');
