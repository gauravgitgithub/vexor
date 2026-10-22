#!/usr/bin/env node
// Assemble publishable npm packages from built binaries.
//
//   node npm/scripts/stage.js --dist DIR --out DIR [--allow-missing]
//
// DIR must contain <rust-target>/vexor for each target below. The version
// comes from the workspace Cargo.toml, and is stamped into every package and
// into vexord's optionalDependencies. Release tooling only; not shipped.

'use strict';

const fs = require('fs');
const path = require('path');

const ROOT = path.resolve(__dirname, '..', '..');
const TARGETS = {
  'x86_64-unknown-linux-musl': 'linux-x64',
  'aarch64-unknown-linux-musl': 'linux-arm64',
  'aarch64-apple-darwin': 'darwin-arm64',
  'x86_64-apple-darwin': 'darwin-x64',
};

function die(msg) {
  console.error(`stage: ${msg}`);
  process.exit(1);
}

function args() {
  const a = process.argv.slice(2);
  const get = (name) => {
    const i = a.indexOf(name);
    return i >= 0 ? a[i + 1] : undefined;
  };
  const dist = get('--dist');
  const out = get('--out');
  if (!dist || !out) die('usage: stage.js --dist DIR --out DIR [--allow-missing]');
  return { dist: path.resolve(dist), out: path.resolve(out), allowMissing: a.includes('--allow-missing') };
}

function cargoVersion() {
  const toml = fs.readFileSync(path.join(ROOT, 'Cargo.toml'), 'utf8');
  const section = toml.split('[workspace.package]')[1] || '';
  const m = section.match(/^version\s*=\s*"([^"]+)"/m);
  if (!m) die('no version in [workspace.package] of Cargo.toml');
  return m[1];
}

function readJson(p) {
  return JSON.parse(fs.readFileSync(p, 'utf8'));
}

function writeJson(p, v) {
  fs.writeFileSync(p, `${JSON.stringify(v, null, 2)}\n`);
}

function copyLicenses(dir) {
  for (const f of ['LICENSE-MIT', 'LICENSE-APACHE']) {
    fs.copyFileSync(path.join(ROOT, f), path.join(dir, f));
  }
}

function main() {
  const { dist, out, allowMissing } = args();
  const version = cargoVersion();
  fs.rmSync(out, { recursive: true, force: true });
  fs.mkdirSync(out, { recursive: true });

  // Consistency: launcher table, optionalDependencies and TARGETS agree.
  const wrapperSrc = path.join(ROOT, 'npm', 'vexord');
  const launcher = fs.readFileSync(path.join(wrapperSrc, 'bin', 'vexor.js'), 'utf8');
  const wrapper = readJson(path.join(wrapperSrc, 'package.json'));
  const expected = Object.values(TARGETS).map((p) => `vexord-${p}`).sort();
  const optional = Object.keys(wrapper.optionalDependencies).sort();
  if (JSON.stringify(expected) !== JSON.stringify(optional)) {
    die(`optionalDependencies ${optional} don't match targets ${expected}`);
  }
  for (const pkg of expected) {
    if (!launcher.includes(`'${pkg}'`)) die(`bin/vexor.js doesn't know ${pkg}`);
  }

  const staged = [];
  for (const [target, platform] of Object.entries(TARGETS)) {
    const binary = path.join(dist, target, 'vexor');
    if (!fs.existsSync(binary)) {
      if (allowMissing) {
        console.error(`stage: skipping ${platform}: no ${binary}`);
        continue;
      }
      die(`missing ${binary}`);
    }
    const src = path.join(ROOT, 'npm', 'platforms', platform);
    const dir = path.join(out, `vexord-${platform}`);
    fs.mkdirSync(path.join(dir, 'bin'), { recursive: true });
    const pkg = readJson(path.join(src, 'package.json'));
    pkg.version = version;
    writeJson(path.join(dir, 'package.json'), pkg);
    fs.copyFileSync(binary, path.join(dir, 'bin', 'vexor'));
    fs.chmodSync(path.join(dir, 'bin', 'vexor'), 0o755);
    copyLicenses(dir);
    staged.push(dir);
  }
  if (staged.length === 0) die('no binaries found');

  const dir = path.join(out, 'vexord');
  fs.mkdirSync(path.join(dir, 'bin'), { recursive: true });
  wrapper.version = version;
  for (const dep of Object.keys(wrapper.optionalDependencies)) {
    wrapper.optionalDependencies[dep] = version;
  }
  writeJson(path.join(dir, 'package.json'), wrapper);
  fs.copyFileSync(path.join(wrapperSrc, 'bin', 'vexor.js'), path.join(dir, 'bin', 'vexor.js'));
  fs.chmodSync(path.join(dir, 'bin', 'vexor.js'), 0o755);
  fs.copyFileSync(path.join(wrapperSrc, 'README.md'), path.join(dir, 'README.md'));
  copyLicenses(dir);
  staged.push(dir);

  console.log(JSON.stringify({ version, packages: staged }, null, 2));
}

main();
