import { chmod, cp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const [, , version, artifactsDirectory = "artifacts", outputDirectory = "npm/dist"] =
  process.argv;

if (!version) {
  console.error("Usage: node npm/prepare-release.mjs <version> [artifacts-dir] [output-dir]");
  process.exit(1);
}

const npmDirectory = dirname(fileURLToPath(import.meta.url));
const artifactsPath = resolve(artifactsDirectory);
const outputPath = resolve(outputDirectory);

const targets = [
  {
    key: "linux-x64",
    packageName: "@nunu-ai/nunu-cli-linux-x64",
    artifact: "nunu-cli-linux-x86_64",
    binary: "nunu-cli"
  },
  {
    key: "darwin-x64",
    packageName: "@nunu-ai/nunu-cli-darwin-x64",
    artifact: "nunu-cli-macos-x86_64",
    binary: "nunu-cli"
  },
  {
    key: "darwin-arm64",
    packageName: "@nunu-ai/nunu-cli-darwin-arm64",
    artifact: "nunu-cli-macos-arm64",
    binary: "nunu-cli"
  },
  {
    key: "win32-x64",
    packageName: "@nunu-ai/nunu-cli-win32-x64",
    artifact: "nunu-cli-windows-x86_64.exe",
    binary: "nunu-cli.exe"
  }
];

async function readJson(path) {
  return JSON.parse(await readFile(path, "utf8"));
}

async function writeJson(path, value) {
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, `${JSON.stringify(value, null, 2)}\n`);
}

await rm(outputPath, { recursive: true, force: true });
await mkdir(outputPath, { recursive: true });

const rootPackage = await readJson(join(npmDirectory, "package.json"));
rootPackage.version = version;
rootPackage.optionalDependencies = Object.fromEntries(
  targets.map((target) => [target.packageName, version])
);
await cp(join(npmDirectory, "bin"), join(outputPath, "bin"), { recursive: true });
await writeJson(join(outputPath, "package.json"), rootPackage);

for (const target of targets) {
  const source = join(artifactsPath, target.artifact);
  const packagePath = join(outputPath, "platforms", target.key);
  const binaryPath = join(packagePath, "bin", target.binary);

  const packageJson = await readJson(
    join(npmDirectory, "platforms", target.key, "package.json")
  );
  packageJson.version = version;
  await writeJson(join(packagePath, "package.json"), packageJson);
  await mkdir(dirname(binaryPath), { recursive: true });
  await cp(source, binaryPath);
  if (process.platform !== "win32") {
    await chmod(binaryPath, 0o755);
  }
}

console.log(`Prepared npm packages for nunu-cli v${version} in ${outputPath}`);
