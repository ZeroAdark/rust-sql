// vfs-link.test.mjs — the VFS hard links behind the WASI path_link import (D4947).
//
// postgres.wasm imports wasi_snapshot_preview1.path_link (pgrcolumnar's spool links table files when
// pgrust.pgrc_parts_in_wal is reference or auto). Without the import the module does not instantiate; with it, a
// second name must share the first one's node, and unlink / rename / filestat must keep the link count right.
//
// Run: node wasm/test/vfs-link.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { Vfs, makeWasi } from '../pgrust-wasi.js';

const E = { SUCCESS: 0, EXIST: 20, NOENT: 44, PERM: 63 };
const fresh = () => new Vfs(new Uint8Array(0), { dirs: ['/d', '/e'], files: [] });

test('a link is a second name for the same node', () => {
  const v = fresh();
  v.create('/d/a').data = new Uint8Array([1, 2, 3]);
  assert.equal(v.link('/d/a', '/e/b'), E.SUCCESS);
  assert.equal(v.get('/e/b'), v.get('/d/a'));
  assert.equal(v.get('/d/a').nlink, 2);
  v.get('/e/b').data = new Uint8Array([9]);
  assert.equal(v.get('/d/a').data[0], 9, 'a write through one name is seen through the other');
});

test('link refuses what link(2) refuses', () => {
  const v = fresh();
  v.create('/d/a');
  v.create('/d/b');
  assert.equal(v.link('/d/a', '/d/b'), E.EXIST);
  assert.equal(v.link('/d/none', '/d/c'), E.NOENT);
  assert.equal(v.link('/d', '/e/x'), E.PERM);
  assert.equal(v.link('/d/a', '/nodir/x'), E.NOENT);
  assert.equal(v.get('/d/a').nlink ?? 1, 1, 'a refused link leaves the count alone');
});

test('unlink drops one name and one count', () => {
  const v = fresh();
  v.create('/d/a');
  v.link('/d/a', '/d/b');
  assert.equal(v.unlink('/d/a'), E.SUCCESS);
  assert.equal(v.get('/d/a'), undefined);
  assert.equal(v.get('/d/b').nlink, 1);
  assert.equal(v.unlink('/d/b'), E.SUCCESS);
  assert.equal(v.get('/d/b'), undefined);
});

test('rename between two names of one node, or a name onto itself, changes nothing', () => {
  const v = fresh();
  v.create('/d/a');
  v.link('/d/a', '/d/b');
  assert.equal(v.rename('/d/a', '/d/b'), E.SUCCESS);
  assert.ok(v.get('/d/a') && v.get('/d/a') === v.get('/d/b'));
  assert.equal(v.get('/d/a').nlink, 2);
  assert.equal(v.rename('/d/a', '/d/a'), E.SUCCESS);
  assert.equal(v.get('/d/a').nlink, 2, 'rename(x, x) keeps the count');
});

test('rename over one name of a linked node drops that node to one name', () => {
  const v = fresh();
  const a = v.create('/d/a');
  v.link('/d/a', '/d/b');
  const g = v.create('/d/g');
  assert.equal(v.rename('/d/g', '/d/b'), E.SUCCESS);
  assert.equal(v.get('/d/b'), g);
  assert.equal(v.get('/d/a'), a);
  assert.equal(a.nlink, 1);
});

test('the WASI import links through the preopen and filestat reports nlink', () => {
  const h = makeWasi({ image: new Uint8Array(0), manifest: { dirs: ['/pgdata', '/pgdata/spool'], files: [] } });
  const memory = new WebAssembly.Memory({ initial: 1 });
  h.setMemory(memory);
  const put = (ptr, s) => { const b = new TextEncoder().encode(s); new Uint8Array(memory.buffer).set(b, ptr); return [ptr, b.length]; };
  h.vfs.create('/pgdata/t');
  const PREOPEN = 3;
  const [fp, fl] = put(0, 'pgdata/t');
  const [tp, tl] = put(64, 'pgdata/spool/pending-t');
  assert.equal(typeof h.wasi.path_link, 'function', 'the module imports path_link');
  assert.equal(h.wasi.path_link(PREOPEN, 0, fp, fl, PREOPEN, tp, tl), E.SUCCESS);
  assert.equal(h.vfs.get('/pgdata/spool/pending-t'), h.vfs.get('/pgdata/t'));
  assert.equal(h.wasi.path_filestat_get(PREOPEN, 0, fp, fl, 256), E.SUCCESS);
  assert.equal(new DataView(memory.buffer).getBigUint64(256 + 24, true), 2n, 'filestat nlink');
});
