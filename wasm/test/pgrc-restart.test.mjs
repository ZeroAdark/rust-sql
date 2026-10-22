import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { normalizeSingleUserInput } from '../format.js';
import { serializeVfs, restoreVfs } from '../snapshot.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const assets = path.join(here, '../assets');
const host = process.env.PGRUST_WASI
  ? pathToFileURL(path.resolve(process.env.PGRUST_WASI)).href
  : new URL('../pgrust-wasi.js', import.meta.url).href;
const { run, Vfs } = await import(host);
const wasmModule = await WebAssembly.compile(fs.readFileSync(
  process.env.PGRUST_WASM || path.join(assets, 'postgres.wasm'),
));
const prefix = process.env.PGRUST_VFS || path.join(assets, 'vfs');
const image = new Uint8Array(fs.readFileSync(prefix + '.img'));
const manifest = JSON.parse(fs.readFileSync(prefix + '.json', 'utf8'));

async function query(vfs, sql) {
  const stdout = [], stderr = [];
  const result = await run({
    wasmModule,
    vfs,
    stdinBytes: new TextEncoder().encode(normalizeSingleUserInput(sql)),
    onStdout: bytes => stdout.push(Buffer.from(bytes)),
    onStderr: bytes => stderr.push(Buffer.from(bytes)),
  });
  const out = Buffer.concat(stdout).toString('utf8');
  const err = Buffer.concat(stderr).toString('utf8');
  assert.equal(result.exitCode, 0, err);
  assert.doesNotMatch(err, /\b(?:ERROR|FATAL|PANIC):/, err);
  return out;
}

async function populatedVfs() {
  const vfs = new Vfs(image.slice(), manifest);
  await query(vfs, `
    create access method pgrcolumnar type table handler heap_tableam_handler;
    create table c (id int) using pgrcolumnar;
    insert into c select generate_series(1,1000);
    create table h (id int);
    insert into h select generate_series(1,1000);
  `);
  return vfs;
}

function assertRows(out) {
  assert.match(out, /count = "1000"/, out);
  assert.match(out, /sum = "500500"/, out);
}

test('columnar rows survive the next single-user invocation', async () => {
  const vfs = await populatedVfs();
  assertRows(await query(vfs, 'select count(*), sum(id) from c;'));
});

test('columnar rows survive snapshot restore and remain writable', async () => {
  const before = await populatedVfs();
  const snapshot = restoreVfs(serializeVfs(before, 1));
  const vfs = new Vfs(snapshot.image, snapshot.manifest);
  assertRows(await query(vfs, 'select count(*), sum(id) from c;'));
  await query(vfs, 'insert into c values (1001);');
  const written = restoreVfs(serializeVfs(vfs, 2));
  const restored = new Vfs(written.image, written.manifest);
  const out = await query(restored, 'select count(*), sum(id) from c;');
  assert.match(out, /count = "1001"/, out);
  assert.match(out, /sum = "501501"/, out);
});

test('heap rows survive single-user invocations and snapshot restore', async () => {
  const before = await populatedVfs();
  assertRows(await query(before, 'select count(*), sum(id) from h;'));
  const snapshot = restoreVfs(serializeVfs(before, 1));
  const vfs = new Vfs(snapshot.image, snapshot.manifest);
  assertRows(await query(vfs, 'select count(*), sum(id) from h;'));
});
