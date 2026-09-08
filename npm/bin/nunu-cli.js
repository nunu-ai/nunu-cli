#!/usr/bin/env node

import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { createRequire } from "node:module";
import { spawn } from "node:child_process";

const require = createRequire(import.meta.url);

const targets = {
  "linux:x64": {
    packageName: "@nunu-ai/nunu-cli-linux-x64",
    binaryName: "nunu-cli"
  },
  "darwin:x64": {
    packageName: "@nunu-ai/nunu-cli-darwin-x64",
    binaryName: "nunu-cli"
  },
  "darwin:arm64": {
    packageName: "@nunu-ai/nunu-cli-darwin-arm64",
    binaryName: "nunu-cli"
  },
  "win32:x64": {
    packageName: "@nunu-ai/nunu-cli-win32-x64",
    binaryName: "nunu-cli.exe"
  }
};

const target = targets[`${process.platform}:${process.arch}`];

if (!target) {
  process.stderr.write(
    `nunu-cli does not currently provide an npm binary for ${process.platform}/${process.arch}.\n`
  );
  process.exit(1);
}

let binaryPath;

try {
  const packageJsonPath = require.resolve(`${target.packageName}/package.json`);
  binaryPath = join(dirname(packageJsonPath), "bin", target.binaryName);
} catch {
  process.stderr.write(
    `nunu-cli could not find its platform package (${target.packageName}). ` +
      "Try reinstalling the package for this platform.\n"
  );
  process.exit(1);
}

if (!existsSync(binaryPath)) {
  process.stderr.write(`nunu-cli binary is missing: ${binaryPath}\n`);
  process.exit(1);
}

const child = spawn(binaryPath, process.argv.slice(2), {
  stdio: "inherit",
  windowsHide: false
});

for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.on(signal, () => {
    if (!child.killed) {
      child.kill(signal);
    }
  });
}

child.on("error", (error) => {
  process.stderr.write(`failed to start nunu-cli: ${error.message}\n`);
  process.exitCode = 1;
});

child.on("close", (code, signal) => {
  process.exitCode = signal ? 1 : code ?? 1;
});
