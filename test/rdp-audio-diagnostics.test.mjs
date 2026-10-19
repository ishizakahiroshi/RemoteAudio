import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
const root = fileURLToPath(new URL('../', import.meta.url));
const fixtures = path.join(root, 'test/fixtures/rdp-audio-diagnostics');
const id = '0123456789abcdef0123456789abcdef';
const candidates = process.platform === 'win32' ? ['pwsh', 'powershell.exe'] : ['pwsh'];
const shell = candidates.find(command => spawnSync(command, ['-NoProfile', '-NonInteractive', '-Command', '$PSVersionTable.PSVersion.Major'], { encoding: 'utf8' }).status === 0);
function invoke(script, args = []) {
  return spawnSync(shell, ['-NoProfile', '-NonInteractive', '-File', path.join(root, script), ...args], { encoding: 'utf8', timeout: 30000 });
}

test('synthetic diagnostics fixtures follow the shared numeric envelope and monotonic order', () => {
  for (const source of ['host', 'guest']) {
    const rows = fs.readFileSync(path.join(fixtures, `${source}.jsonl.fixture`), 'utf8').trimEnd().split('\n').map(JSON.parse);
    let previous = -1, elapsed = -1;
    for (const row of rows) {
      assert.equal(row.schema_version, 1);
      assert.equal(row.source, source);
      assert.equal(row.capture_id, id);
      assert.ok(Number.isSafeInteger(row.sequence) && row.sequence > previous);
      assert.ok(Number.isSafeInteger(row.elapsed_ms) && row.elapsed_ms >= elapsed);
      assert.ok(Number.isSafeInteger(row.utc_unix_ms));
      if (row.event === 'meter_sample') {
        assert.ok(Number.isSafeInteger(row.acquired_elapsed_ms) && row.acquired_elapsed_ms <= row.elapsed_ms);
        assert.ok(Number.isSafeInteger(row.acquired_utc_unix_ms));
      }
      previous = row.sequence; elapsed = row.elapsed_ms;
      for (const key of ['token', 'ip', 'hostname', 'username', 'message', 'device_id', 'friendly_name', 'EventData']) assert.ok(!(key in row));
    }
  }
});

test('PowerShell analyzer/collector synthetic integration (requires PowerShell)', { skip: !shell }, () => {
  const result = invoke('test/rdp-audio-diagnostics.synthetic.ps1');
  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
});

test('PowerShell analyzer refuses overwriting evidence (requires PowerShell)', { skip: !shell }, () => {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'rdp-diagnostic-test-'));
  try {
    const output = path.join(temp, 'existing.json');
    fs.writeFileSync(output, 'existing evidence');
    const result = invoke('scripts/analyze-rdp-audio-diagnostics.ps1', ['-InputPath', path.join(fixtures, 'host.jsonl.fixture'), '-CaptureId', id, '-OutputPath', output]);
    assert.notEqual(result.status, 0);
    assert.equal(fs.readFileSync(output, 'utf8'), 'existing evidence');
  } finally { fs.rmSync(temp, { recursive: true, force: true }); }
});
