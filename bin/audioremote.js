#!/usr/bin/env node

// npm entry point. The published `audioremote` package carries no native code:
// it depends on a per-platform package (`optionalDependencies`) that ships the
// Rust executable, resolves it here, and hands over stdio and the exit code.
//
// The resolution steps are exported so `test/launcher.test.mjs` can exercise
// them directly — this file is the primary install channel, and a typo in a
// package name or a swallowed exit code would otherwise only be discovered by
// users after publish.

import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { pathToFileURL } from "node:url";

const require = createRequire(import.meta.url);

/** Platform key -> [npm package, executable name inside its `bin/` folder]. */
export const PLATFORM_PACKAGES = {
  "win32-x64": ["@ishizakahiroshi/audioremote-win32-x64", "RemoteAudio.exe"],
};

export function platformKey(platform = process.platform, arch = process.arch) {
  return `${platform}-${arch}`;
}

export class UnsupportedPlatform extends Error {
  constructor(key) {
    super(`unsupported platform ${key} (Windows 11 x64 only)`);
    this.name = "UnsupportedPlatform";
  }
}

export class MissingNativePackage extends Error {
  constructor(packageName) {
    super(
      `native package ${packageName} is missing. Reinstall audioremote without --no-optional.`,
    );
    this.name = "MissingNativePackage";
  }
}

/** Absolute path of the native executable for `key`.
 *
 *  `AUDIOREMOTE_BINARY_PATH` wins over everything, including the platform check:
 *  it exists so a local `cargo build --release` can be driven through the real
 *  launcher, and an explicit override should not be second-guessed. */
export function resolveBinary(key, options = {}) {
  const env = options.env ?? process.env;
  const resolve = options.resolve ?? ((id) => require.resolve(id));

  const override = env.AUDIOREMOTE_BINARY_PATH;
  if (override) return override;

  const selected = PLATFORM_PACKAGES[key];
  if (!selected) throw new UnsupportedPlatform(key);

  const [packageName, binaryName] = selected;
  try {
    const manifest = resolve(`${packageName}/package.json`);
    return join(dirname(manifest), "bin", binaryName);
  } catch {
    throw new MissingNativePackage(packageName);
  }
}

/** Run the native binary and return the exit code this process should use. */
export function main(argv = process.argv.slice(2)) {
  let binary;
  try {
    binary = resolveBinary(platformKey());
  } catch (error) {
    process.stderr.write(`audioremote: ${error.message}\n`);
    return 2;
  }

  const result = spawnSync(binary, argv, { stdio: "inherit", windowsHide: true });
  if (result.error) {
    process.stderr.write(
      `audioremote: failed to start native binary: ${result.error.message}\n`,
    );
    return 2;
  }
  if (result.signal) {
    // Re-raise the signal so wrappers see the real cause of death, not an exit code.
    process.kill(process.pid, result.signal);
    return 0;
  }
  return result.status ?? 2;
}

const invokedDirectly =
  process.argv[1] !== undefined &&
  import.meta.url === pathToFileURL(process.argv[1]).href;
if (invokedDirectly) {
  process.exit(main());
}
