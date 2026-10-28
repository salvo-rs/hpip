const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

function management() {
  const requests = [];
  const location = { origin: 'https://example.test', pathname: '/base%20folder/', reload() {} };
  class Request {
    constructor() { this.headers = {}; this.listeners = {}; }
    addEventListener(name, callback) { this.listeners[name] = callback; }
    open(method, url) { this.method = method; this.url = new URL(url); }
    setRequestHeader(name, value) { this.headers[name] = value; }
    send() { requests.push(this); }
  }
  const context = vm.createContext({ window: { location }, XMLHttpRequest: Request });
  vm.runInContext(fs.readFileSync(path.join(__dirname, '../../assets/manage.js'), 'utf8'), context);
  return { context, requests, status: { textContent: '', classList: { add() {} } } };
}

test('rename preserves reserved characters in both exact file names', () => {
  for (const [source, destination] of [['report.txt#draft', 'new#name'], ['a%20b', 'a?b'], ['空 格.txt', '新 名.txt']]) {
    const { context, requests, status } = management();
    context.rename(source, destination, status);
    assert.equal(requests.length, 1);
    const request = requests[0];
    assert.equal(request.method, 'MOVE');
    assert.equal(request.url.pathname, '/base%20folder/' + encodeURIComponent(source));
    assert.equal(request.url.hash, '');
    assert.equal(request.url.search, '');
    const target = new URL(request.headers.Destination);
    assert.equal(target.pathname, '/base%20folder/' + encodeURIComponent(destination));
    assert.equal(target.hash, '');
    assert.equal(target.search, '');
  }
});

test('directory creation encodes a single file name and keeps the parent URL', () => {
  const { context, requests, status } = management();
  context.create_new_directory('new#directory?%', status);
  assert.equal(requests.length, 1);
  assert.equal(requests[0].method, 'MKCOL');
  assert.equal(requests[0].url.pathname, '/base%20folder/new%23directory%3F%25');
  assert.equal(requests[0].url.hash, '');
});

test('invalid basenames do not send requests to a normalized parent or child', () => {
  for (const name of ['', '.', '..', '../other', 'child/name', 'child\\name']) {
    const { context, requests, status } = management();
    context.rename('source.txt', name, status);
    context.create_new_directory(name, status);
    assert.equal(requests.length, 0);
    assert.match(status.textContent, /file name/);
  }
});

test('management errors remain text rather than parsed response markup', () => {
  const { context, requests, status } = management();
  Object.defineProperty(status, 'innerHTML', { set() { throw new Error('unsafe HTML insertion'); } });
  context.rename('source', 'target', status);
  const request = requests[0];
  Object.assign(request, { status: 404, statusText: 'Not Found', response: '<img data-review=marker>' });
  request.listeners.loadend();
  assert.equal(status.textContent, '404 Not Found — <img data-review=marker>');
});
