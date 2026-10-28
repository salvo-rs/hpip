const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

function uploader() {
  class Element {
    constructor(tag) { this.tagName = tag.toUpperCase(); this.children = []; this.listeners = {}; }
    appendChild(child) { child.parentNode = this; child.parentElement = this; this.children.push(child); return child; }
    insertBefore(child) { return this.appendChild(child); }
    addEventListener(name, callback) { this.listeners[name] = callback; }
    removeEventListener(name) { delete this.listeners[name]; }
    removeAttribute(name) { delete this[name]; }
    remove() { this.removed = true; }
  }
  const body = new Element('body');
  const input = body.appendChild(new Element('input'));
  const requests = [];
  let reloads = 0;
  let ready;
  const location = { pathname: '/base/', reload() { reloads++; } };
  class Request {
    constructor() { this.listeners = {}; this.upload = new Element('upload'); this.headers = {}; }
    addEventListener(name, callback) { this.listeners[name] = callback; }
    open(method, url) { this.method = method; this.url = url; }
    setRequestHeader(name, value) { this.headers[name] = value; }
    send(file) { this.file = file; requests.push(this); }
  }
  const document = {
    location,
    getElementsByTagName() { return [body]; },
    querySelector() { return input; },
    createElement(tag) { return new Element(tag); },
    createTextNode(data) { return { data }; },
  };
  const context = vm.createContext({ document, window: { location, addEventListener(name, callback) { if (name === 'DOMContentLoaded') ready = callback; } }, XMLHttpRequest: Request });
  vm.runInContext(fs.readFileSync(path.join(__dirname, '../../assets/upload.js'), 'utf8'), context);
  ready();
  return { body, input, requests, reloads: () => reloads, complete(request, status = 201) { Object.assign(request, { status, response: status === 201 ? '' : 'failed' }); request.listeners.loadend({}); } };
}

test('upload progress uses its supplied event without a legacy global event', () => {
  const page = uploader();
  page.input.files = [{ name: 'file.txt' }];
  page.input.listeners.change();
  const request = page.requests[0];
  request.upload.listeners.loadstart({ timeStamp: 0 });
  assert.doesNotThrow(() => request.upload.listeners.progress({ lengthComputable: true, loaded: 512, total: 1024, timeStamp: 1000 }));
  const list = page.body.children.find(child => child.tagName === 'DL');
  const progress = list.children[1].children[0].children[0];
  assert.equal(progress.value, 512);
  assert.equal(progress.max, 1024);
});

test('folder upload waits for delayed file discovery and every reader batch', () => {
  const page = uploader();
  const reads = [];
  const files = [];
  const folder = { isFile: false, fullPath: '/folder', createReader() { return { readEntries(ok, fail) { reads.push({ ok, fail }); } }; } };
  page.body.listeners.drop({ preventDefault() {}, dataTransfer: { types: ['Files'], files: [{ name: 'folder' }], items: [{ kind: 'file', webkitGetAsEntry() { return folder; } }] } });
  reads.shift().ok(['one.txt', 'two.txt'].map(name => ({ isFile: true, fullPath: '/folder/' + name, file(ok, fail) { files.push({ name, ok, fail }); } })));
  const first = files.shift();
  first.ok({ name: first.name });
  page.complete(page.requests[0]);
  assert.equal(page.reloads(), 0, 'a first completed file must not cancel undiscovered files');
  const second = files.shift();
  second.ok({ name: second.name });
  page.complete(page.requests[1]);
  assert.equal(page.reloads(), 0, 'the reader has not signaled its final batch');
  reads.shift().ok([]);
  assert.equal(page.reloads(), 1);
  assert.deepEqual(page.requests.map(request => request.url), ['/base/folder/one.txt', '/base/folder/two.txt']);
});

test('folder discovery failure remains visible instead of reporting upload completion', () => {
  const page = uploader();
  let fail;
  const folder = { isFile: false, fullPath: '/folder', createReader() { return { readEntries(ok, error) { fail = error; } }; } };
  page.body.listeners.drop({ preventDefault() {}, dataTransfer: { types: ['Files'], files: [{ name: 'folder' }], items: [{ kind: 'file', webkitGetAsEntry() { return folder; } }] } });
  assert.equal(typeof fail, 'function');
  fail(new Error('permission denied'));
  assert.equal(page.reloads(), 0);
  const list = page.body.children.find(child => child.tagName === 'DL');
  assert.match(list.children.at(-1).innerText, /folder.*permission denied/);
});
