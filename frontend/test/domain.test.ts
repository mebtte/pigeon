import { strict as assert } from 'node:assert';
import { test } from 'node:test';
import { appPath, formatBytes, formatUploadLimit, makePassword, readBootstrap, uploadForm } from '../src/domain.ts';
import { readRoute, shareLinkWithPassword } from '../src/routes.ts';

test('loads the base path from the backend bootstrap', () => {
  const bootstrap = { base_path: '/pigeon/team' };
  const encoded = Buffer.from(JSON.stringify(bootstrap)).toString('base64url');
  assert.deepEqual(readBootstrap(encoded), bootstrap);
  assert.deepEqual(readBootstrap('__PIGEON_BOOTSTRAP__'), { base_path: '/' });
  assert.deepEqual(readBootstrap(null), { base_path: '/' });
});

test('keeps links inside the configured base path', () => {
  assert.equal(appPath('/', '/api/config'), '/api/config');
  assert.equal(appPath('/pigeon', '/api/config'), '/pigeon/api/config');
  assert.equal(appPath('/pigeon'), '/pigeon');
});

const shareId = 'Ab2-_k';
const shareHash = `#/s/${shareId}`;

test('reads home, login and six-character sharing hash routes', () => {
  assert.deepEqual(readRoute(''), { page: 'home' });
  assert.deepEqual(readRoute('#/'), { page: 'home' });
  assert.deepEqual(readRoute('#/login'), { page: 'login' });
  assert.deepEqual(readRoute(shareHash), { page: 'sharing', id: shareId, password: '' });
  for (const hash of [
    '#/login/', '#/unknown', '#p=secret',
    `#${shareId}`, `#/sharing/${shareId}`, `${shareHash}/`,
    shareHash.slice(0, -1), `${shareHash}X`, shareHash.replace('-', '+'),
  ]) {
    assert.deepEqual(readRoute(hash), { page: 'notFound' }, hash);
  }
});

test('round-trips encoded share passwords in root and nested hash links', () => {
  const password = 'a+b&c=#?';
  for (const base of ['/', '/pigeon/team/']) {
    const link = `https://example.com${base}${shareHash}`;
    const url = new URL(shareLinkWithPassword(link, password));
    assert.equal(url.pathname, base);
    assert.equal(url.search, '');
    assert.equal(url.hash, `${shareHash}?p=a%2Bb%26c%3D%23%3F`);
    assert.deepEqual(readRoute(url.hash), { page: 'sharing', id: shareId, password });
    assert.equal(shareLinkWithPassword(url.href, ''), link);
  }
});

test('omits cleared optional upload settings and sends configured values', () => {
  const file = new File(['hello'], 'test.txt');
  const unlimited = uploadForm(file, { expiration: '', maxDownloads: '' });
  assert.deepEqual(Array.from(unlimited.keys()), ['file']);
  assert.equal(unlimited.get('file'), file);

  const limited = uploadForm(file, { expiration: '7', maxDownloads: '2', password: 'ABCDEFGH' });
  assert.equal(limited.get('expiry_days'), '7');
  assert.equal(limited.get('max_downloads'), '2');
  assert.equal(limited.get('password'), 'ABCDEFGH');
});

test('generates five-character passwords with unambiguous letters and digits', () => {
  for (let i = 0; i < 64; i += 1) assert.match(makePassword(), /^[2-9A-HJ-NP-Z]{5}$/);
});

test('formats filesystem space and file sizes in IEC units', () => {
  assert.equal(formatBytes(0), '0 B');
  assert.equal(formatBytes(1024), '1 KiB');
  assert.equal(formatBytes(1024 ** 3 * 1.5), '1.5 GiB');
});

test('formats upload limits in decimal MB and GB with byte precision', () => {
  assert.equal(formatUploadLimit(128), '0.000128 MB');
  assert.equal(formatUploadLimit(1_048_576), '1.048576 MB');
  assert.equal(formatUploadLimit(500_000_000), '500 MB');
  assert.equal(formatUploadLimit(1_000_000_001), '1.000000001 GB');
  assert.equal(formatUploadLimit(1_500_000_000), '1.5 GB');
});
