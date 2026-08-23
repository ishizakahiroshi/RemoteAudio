// Tests for the npm launcher (bin/audioremote.js) — the primary install channel.
//
// The end-to-end cases use `process.execPath` (node itself) as a stand-in native
// binary via AUDIOREMOTE_BINARY_PATH, so they need no fixtures and run on any
// platform, including a Linux CI runner where the real .exe cannot execute.

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  MissingNativePackage,
  PLATFORM_PACKAGES,
  UnsupportedPlatform,
  platformKey,
  resolveBinary,
} from "../bin/audioremote.js";

const here = dirname(fileURLToPath(import.meta.url));
const launcher = join(here, "..", "bin", "audioremote.js");

/** Run the launcher itself, with a fake native binary. */
function runLauncher(args, binaryPath) {
  return spawnSync(process.execPath, [launcher, ...args], {
    encoding: "utf8",
    env: { ...process.env, AUDIOREMOTE_BINARY_PATH: binaryPath },
  });
}

test("the platform map matches the published package names", () => {
  assert.deepEqual(Object.keys(PLATFORM_PACKAGES), ["win32-x64"]);
  assert.deepEqual(PLATFORM_PACKAGES["win32-x64"], [
    "@ishizakahiroshi/audioremote-win32-x64",
    "RemoteAudio.exe",
  ]);
});

test("platformKey joins platform and arch the way the map is keyed", () => {
  assert.equal(platformKey("win32", "x64"), "win32-x64");
  assert.equal(platformKey("linux", "arm64"), "linux-arm64");
});

test("a supported platform resolves to <package>/bin/<exe>", () => {
  const resolved = resolveBinary("win32-x64", {
    env: {},
    resolve: (id) => {
      assert.equal(id, "@ishizakahiroshi/audioremote-win32-x64/package.json");
      return join("C:", "node_modules", "@ishizakahiroshi", "audioremote-win32-x64", "package.json");
    },
  });
  assert.equal(
    resolved,
    join("C:", "node_modules", "@ishizakahiroshi", "audioremote-win32-x64", "bin", "RemoteAudio.exe"),
  );
});

test("an unsupported platform is refused by name", () => {
  assert.throws(
    () => resolveBinary("linux-x64", { env: {}, resolve: () => "unused" }),
    (error) => {
      assert.ok(error instanceof UnsupportedPlatform);
      assert.match(error.message, /unsupported platform linux-x64/);
      return true;
    },
  );
});

test("a missing optional package tells the user how to fix it", () => {
  assert.throws(
    () =>
      resolveBinary("win32-x64", {
        env: {},
        resolve: () => {
          throw new Error("Cannot find module");
        },
      }),
    (error) => {
      assert.ok(error instanceof MissingNativePackage);
      assert.match(error.message, /@ishizakahiroshi\/audioremote-win32-x64 is missing/);
      assert.match(error.message, /--no-optional/);
      return true;
    },
  );
});

test("AUDIOREMOTE_BINARY_PATH overrides resolution entirely", () => {
  const resolved = resolveBinary("linux-x64", {
    env: { AUDIOREMOTE_BINARY_PATH: "/tmp/audioremote" },
    resolve: () => {
      throw new Error("resolve must not be consulted when the override is set");
    },
  });
  assert.equal(resolved, "/tmp/audioremote");
});

test("a non-zero exit code from the native binary is propagated", () => {
  const result = runLauncher(["-e", "process.exit(7)"], process.execPath);
  assert.equal(result.status, 7);
});

test("a successful run exits 0", () => {
  const result = runLauncher(["-e", "process.exit(0)"], process.execPath);
  assert.equal(result.status, 0);
});

test("arguments are forwarded to the native binary verbatim", () => {
  const result = runLauncher(
    ["-e", "process.stdout.write(process.argv.slice(1).join('|'))", "serve", "--no-open"],
    process.execPath,
  );
  assert.equal(result.status, 0);
  assert.equal(result.stdout, "serve|--no-open");
});

test("a binary that cannot be started exits 2 with an explanation", () => {
  const result = runLauncher([], join(here, "definitely-not-a-real-binary"));
  assert.equal(result.status, 2);
  assert.match(result.stderr, /failed to start native binary/);
});
