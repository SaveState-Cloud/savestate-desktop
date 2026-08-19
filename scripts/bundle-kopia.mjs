import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdirSync, rmSync, writeFileSync, copyFileSync, readdirSync, statSync, chmodSync } from "node:fs";
import { join, resolve } from "node:path";

const repoRoot = resolve(process.cwd());
const binDir = resolve(repoRoot, "src-tauri", "bin");
const tmpDir = resolve(repoRoot, "src-tauri", ".kopia-tmp");

const rawVersion = process.env.KOPIA_VERSION || "v0.23.1";
const version = rawVersion.startsWith("v") ? rawVersion.slice(1) : rawVersion;
const tag = `v${version}`;

const isWindows = process.platform === "win32";
const isLinux = process.platform === "linux";

if (!isWindows && !isLinux) {
  console.log(`[bundle-kopia] Skipping on unsupported platform: ${process.platform}`);
  process.exit(0);
}

const assetName = isWindows
  ? `kopia-${version}-windows-x64.zip`
  : `kopia-${version}-linux-x64.tar.gz`;
const url = `https://github.com/kopia/kopia/releases/download/${tag}/${assetName}`;
const knownChecksums = {
  "kopia-0.23.1-windows-x64.zip": "f0369d9657dbe47a1b8b6ff4e308c5958a4813b405d4e32ae9d81b3f2b3d8251",
  "kopia-0.23.1-linux-x64.tar.gz": "416d0f84a3dbb321a8b2d8f0997b1a0a6e915babe79ee76fa6e4d2bd1e1c5178",
};
const expectedChecksum = process.env.KOPIA_SHA256 || knownChecksums[assetName];

if (!expectedChecksum) {
  throw new Error(`No trusted SHA-256 configured for ${assetName}`);
}

mkdirSync(binDir, { recursive: true });
rmSync(tmpDir, { recursive: true, force: true });
mkdirSync(tmpDir, { recursive: true });

const archivePath = join(tmpDir, assetName);
console.log(`[bundle-kopia] Downloading ${url}`);
const resp = await fetch(url);
if (!resp.ok) {
  throw new Error(`Failed to download ${assetName}: ${resp.status}`);
}
const bytes = Buffer.from(await resp.arrayBuffer());
const actualChecksum = createHash("sha256").update(bytes).digest("hex");
if (actualChecksum !== expectedChecksum.toLowerCase()) {
  throw new Error(`SHA-256 mismatch for ${assetName}: expected ${expectedChecksum}, got ${actualChecksum}`);
}
writeFileSync(archivePath, bytes);

console.log("[bundle-kopia] Extracting archive");
if (isWindows) {
  execFileSync("tar", ["-xf", archivePath, "-C", tmpDir], { stdio: "inherit" });
} else {
  execFileSync("tar", ["-xzf", archivePath, "-C", tmpDir], { stdio: "inherit" });
}

function findExecutable(startDir, fileName) {
  const entries = readdirSync(startDir);
  for (const entry of entries) {
    const full = join(startDir, entry);
    const st = statSync(full);
    if (st.isDirectory()) {
      const found = findExecutable(full, fileName);
      if (found) return found;
    } else if (entry === fileName) {
      return full;
    }
  }
  return null;
}

const sourceExe = findExecutable(tmpDir, isWindows ? "kopia.exe" : "kopia");
if (!sourceExe) {
  throw new Error(`Could not find Kopia executable in extracted archive (${assetName})`);
}

const targetExe = join(binDir, isWindows ? "kopia.exe" : "kopia");
copyFileSync(sourceExe, targetExe);
if (!isWindows) {
  chmodSync(targetExe, 0o755);
}

console.log(`[bundle-kopia] Bundled ${targetExe}`);
