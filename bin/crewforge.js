#!/usr/bin/env node
'use strict';

const { execFileSync } = require('child_process');

const PACKAGES = {
  'linux-x64': '@crewforge/linux-x64',
  'linux-arm64': '@crewforge/linux-arm64',
  'darwin-x64': '@crewforge/darwin-x64',
  'darwin-arm64': '@crewforge/darwin-arm64',
  'win32-x64': '@crewforge/win32-x64',
};

const key = `${process.platform}-${process.arch}`;
const pkg = PACKAGES[key];

if (!pkg) {
  process.stderr.write(`[crewforge] Unsupported platform: ${key}\n`);
  process.exit(1);
}

const binName = process.platform === 'win32' ? 'crewforge.exe' : 'crewforge';

let binaryPath;
try {
  binaryPath = require.resolve(`${pkg}/${binName}`);
} catch {
  process.stderr.write(
    `[crewforge] Platform binary not found (${pkg}).\n` +
      'Try reinstalling: npm i -g crewforge\n',
  );
  process.exit(1);
}

try {
  execFileSync(binaryPath, process.argv.slice(2), { stdio: 'inherit' });
} catch (error) {
  process.exit(typeof error.status === 'number' ? error.status : 1);
}
