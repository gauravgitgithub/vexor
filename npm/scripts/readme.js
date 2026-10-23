#!/usr/bin/env node
// Build the npm package README from the repository README: npm can't
// resolve relative links, so they point at GitHub (pages) and jsDelivr
// (images, served with an image content type).
//
//   node npm/scripts/readme.js [ref]      # ref: tag or branch, default main

'use strict';

const fs = require('fs');
const path = require('path');

const ref = process.argv[2] || 'main';
const repo = 'gauravgitgithub/vexor';
const root = path.join(__dirname, '..', '..');
let text = fs.readFileSync(path.join(root, 'README.md'), 'utf8');
// Images (src="docs/..." and ![](docs/...)).
text = text.replace(/src="((?:docs|benchmarks)\/[^"]+)"/g, (_, p) => `src="https://cdn.jsdelivr.net/gh/${repo}@${ref}/${p}"`);
// Relative Markdown links to files and directories.
text = text.replace(/\]\(((?!https?:|#)[^)]+)\)/g, (_, p) => `](https://github.com/${repo}/blob/${ref}/${p.replace(/^\.\//, '')})`);
fs.writeFileSync(path.join(root, 'npm', 'vexord', 'README.md'), text);
