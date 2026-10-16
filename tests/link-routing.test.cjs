const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../web/app.js'), 'utf8');
const routing = source.slice(source.indexOf('function videoPlatform('), source.indexOf('function updateSubmitButton('));
const sharedCases = JSON.parse(fs.readFileSync(path.join(__dirname, '../crates/ffdm-resolver/tests/fixtures/link-routing.json'), 'utf8'));
const labels = {'Qishui Music':'汽水音乐', Douyin:'抖音', Xiaohongshu:'小红书', 'WeChat Channels':'视频号', Kuaishou:'快手', Weibo:'微博', CCTV:'央视网', Ixigua:'西瓜视频', PearVideo:'梨视频', WikimediaCommons:'Wikimedia Commons', Sohu:'搜狐视频', Haokan:'好看视频', HuyaVideo:'虎牙点播', NetEaseNews:'网易新闻'};
function parser() {
  const context = vm.createContext({URL, atob, TextEncoder});
  vm.runInContext(routing, context, {filename:'web/app.js'});
  return context;
}

test('automatic routing agrees with the shared Rust/JavaScript content-page fixtures', () => {
  const {videoPlatform} = parser();
  for (const {url, platform} of sharedCases) {
    assert.equal(videoPlatform(url), labels[platform] || platform, url);
  }
});

test('complete file URLs keep their exact punctuation and signed query strings', () => {
  const {videoUrl} = parser();
  for (const url of [
    'https://files.example.org/file(name)',
    'https://files.example.org/file?signature=abc);',
    'https://files.example.org/file;version=2;',
    'https://files.example.org/owner\'s-file.zip',
    'https://files.example.org/file\'',
    'https://files.example.org/download?name=a%2Bb&signature=a%2Fz%3D!',
  ]) assert.equal(videoUrl(` ${url} `), url);
});

test('share text and Markdown wrappers still yield usable media and file URLs', () => {
  const {videoUrl, videoPlatform} = parser();
  assert.equal(videoPlatform('分享视频 https://v.douyin.com/abc123/。 复制打开'), '抖音');
  assert.equal(videoUrl('[下载](https://files.example.org/file(name).zip)'), 'https://files.example.org/file(name).zip');
  assert.equal(videoUrl('文件（https://files.example.org/file(name)）'), 'https://files.example.org/file(name)');
  assert.equal(videoUrl('链接 https://files.example.org/file?token=example;'), 'https://files.example.org/file?token=example;');
});

test('manual file mode overrides known video pages; explicit parser mode does not require a guessed extension', () => {
  const {mediaPlatform} = parser();
  assert.equal(mediaPlatform('https://x.com/user/status/1234567890', 'direct'), null);
  assert.equal(mediaPlatform('https://ton.x.com/archive-example.zip', 'auto'), null);
  assert.equal(mediaPlatform('https://x.com/user/status/1234567890', 'auto'), 'X');
  assert.equal(mediaPlatform('https://www.xiaohongshu.com/unknown-page', 'media'), '小红书');
  assert.equal(mediaPlatform('https://ton.x.com/archive-example.zip', 'media'), '网页资源');
  assert.equal(mediaPlatform('https://media.example.org/watch/abc', 'media'), '网页资源');
  assert.equal(mediaPlatform('https://music.douyin.com/unknown-page', 'media'), '网页资源');
  assert.equal(mediaPlatform('http://music.douyin.com/qishui/share/track?track_id=1234567890123456789', 'media'), '网页资源');
  assert.equal(mediaPlatform('https://embed-ssl.ted.com/talks/ken_robinson_says_schools_kill_creativity', 'media'), 'TED');
  assert.equal(mediaPlatform('https://commons.wikimedia.org/wiki/file:Sample_video.webm', 'media'), 'Wikimedia Commons');
});

function downloadForm({url, mode = 'auto', preview = null}) {
  const context = parser();
  const fields = new Map();
  const calls = [];
  const field = selector => {
    if (!fields.has(selector)) fields.set(selector, {
      value:'', open:true, textContent:'', hidden:false,
      listeners:new Map(),
      addEventListener(event, callback) { this.listeners.set(event, callback); if (selector === '#new-form' && event === 'submit') context.submit = callback; },
      close() { this.open = false; this.listeners.get('close')?.(); },
      showModal() { this.open = true; },
      focus() {},
      setAttribute() {},
    });
    return fields.get(selector);
  };
  field('#download-url').value = url;
  field('#download-mode').value = mode;
  field('#download-filename').value = 'file.zip';
  field('#download-connections').value = '4';
  field('#media-format').value = 'format-1';
  Object.assign(context, {
    $:field, submitting:false, connected:true,
    mediaSite:context.mediaPlatform(url, mode), mediaPreview:preview,
    filenameVersion:1, filenameTimer:null, filenameSuggestion:{extension:'zip'}, filenameEdited:false,
    clearTimeout() {}, completeFilenameInput() {}, setFilter() {}, toast() {},
    refresh:async () => {}, updateSubmitButton() {},
    resolveFilename:async () => calls.push(['filename']),
    parseVideo:async () => calls.push(['resolveMedia']),
    api: {
      create:async input => { calls.push(['create', input]); return {filename:'file.zip'}; },
      createMedia:async input => { calls.push(['createMedia', input]); return {filename:'video.mp4'}; },
    },
  });
  vm.runInContext(source.slice(source.indexOf("$('#new-dialog').addEventListener('close'"), source.indexOf('\nfunction setFilter(')), context);
  return {context, calls, field};
}

test('form submits arbitrary files to the file API without changing their URLs', async () => {
  for (const [url, mode, direct] of [
    ['https://ton.x.com/i/ton/data/archives/example/archive-example.zip', 'auto', false],
    ['https://files.example.org/download?signature=example);', 'auto', false],
    ['https://x.com/user/status/1234567890', 'direct', true],
  ]) {
    const {context, calls} = downloadForm({url, mode});
    await context.submit({preventDefault() {}});
    assert.equal(calls.length, 1);
    assert.equal(calls[0][0], 'create');
    assert.equal(calls[0][1].url, url);
    assert.equal(calls[0][1].direct, direct);
    assert.equal('cookie' in calls[0][1], false);
  }
});

test('form parses media first and then creates a task from the selected format', async () => {
  const url = 'https://www.youtube.com/watch?v=abcdefghijk';
  const pending = downloadForm({url});
  await pending.context.submit({preventDefault() {}});
  assert.equal(pending.calls[0][0], 'resolveMedia');
  const selected = downloadForm({url, preview:{id:'preview-1'}});
  await selected.context.submit({preventDefault() {}});
  assert.equal(selected.calls[0][0], 'createMedia');
  assert.equal(selected.calls[0][1].preview_id, 'preview-1');
  assert.equal(selected.calls[0][1].format_id, 'format-1');
  assert.equal('cookie' in selected.calls[0][1], false);
});

test('switching to file mode clears stale media previews before filename discovery', () => {
  const {context, field} = downloadForm({url:'https://x.com/user/status/1234567890', preview:{id:'stale'}});
  Object.assign(context, {filenameEdited:false, filenameRequest:null, mediaRequest:null, setTimeout() { return 1; }});
  vm.runInContext(source.slice(source.indexOf('function suggestName('), source.indexOf('function videoPlatform(')), context);
  field('#download-mode').value = 'direct';
  field('#download-cookie').value = 'session=test-only';
  field('#download-headers').value = 'Authorization: Bearer test-only';
  field('#platform-cookie').value = 'platform=test-only';
  context.suggestName();
  assert.equal(context.mediaSite, null);
  assert.equal(context.mediaPreview, null);
  assert.equal(field('#media-source').hidden, true);
  assert.equal(field('#media-panel').hidden, true);
  assert.equal(context.filenameTimer, 1);
  assert.equal(field('#download-cookie').value, '');
  assert.equal(field('#download-headers').value, '');
  assert.equal(field('#platform-cookie').value, '');
});

test('file authorization only reaches the selected HTTPS file request and is cleared after success', async () => {
  const file = downloadForm({url:'https://files.example.org/private.zip'});
  file.context.filenameSuggestion = null;
  file.field('#download-cookie').value = 'session=test-only';
  await file.context.submit({preventDefault() {}});
  assert.equal(file.calls.length, 1, 'authorized requests skip the anonymous filename lookup');
  assert.equal(file.calls[0][0], 'create');
  assert.equal(file.calls[0][1].cookie, 'session=test-only');
  assert.equal(file.field('#download-cookie').value, '');

  const media = downloadForm({url:'https://x.com/user/status/1234567890', preview:{id:'preview-1'}});
  media.field('#download-cookie').value = 'session=test-only';
  media.field('#download-headers').value = 'Authorization: Bearer test-only';
  await media.context.submit({preventDefault() {}});
  assert.equal(media.calls[0][0], 'createMedia');
  assert.equal('cookie' in media.calls[0][1], false);
  assert.equal('headers' in media.calls[0][1], false);
});

test('HTTP authorization is rejected locally; authorized API failures never echo Cookie values', async () => {
  const http = downloadForm({url:'http://files.example.org/private.zip'});
  http.field('#download-cookie').value = 'session=test-only';
  await http.context.submit({preventDefault() {}});
  assert.equal(http.calls.length, 0);
  assert.match(http.field('#form-error').textContent, /HTTPS/);

  const https = downloadForm({url:'https://files.example.org/private.zip'});
  https.field('#download-cookie').value = 'session=test-only';
  https.context.api.create = async () => { throw new Error('server echoed session=test-only'); };
  await https.context.submit({preventDefault() {}});
  assert.equal(https.field('#form-error').textContent.includes('test-only'), false);
  assert.match(https.field('#form-error').textContent, /授权下载未能启动/);
});

test('closing new-download or changing the URL removes all authorization inputs', () => {
  const form = downloadForm({url:'https://files.example.org/private.zip'});
  form.field('#download-cookie').value = 'session=test-only';
  form.field('#download-headers').value = 'Authorization: Bearer test-only';
  form.field('#platform-cookie').value = 'platform=test-only';
  form.field('#new-dialog').close();
  assert.equal(form.field('#download-cookie').value, '');
  assert.equal(form.field('#download-headers').value, '');
  assert.equal(form.field('#platform-cookie').value, '');

  Object.assign(form.context, {filenameEdited:false, filenameRequest:null, mediaRequest:null, setTimeout() { return 1; }});
  vm.runInContext(source.slice(source.indexOf('function suggestName('), source.indexOf('function videoPlatform(')), form.context);
  form.field('#download-cookie').value = 'session=test-only';
  form.field('#download-headers').value = 'X-API-Key: test-only';
  form.field('#download-url').value = 'https://other.example.org/private.zip';
  form.context.suggestName();
  assert.equal(form.field('#download-cookie').value, '');
  assert.equal(form.field('#download-headers').value, '');
  assert.match(form.field('#download-cookie-origin').textContent, /other\.example\.org/);
});

test('YouTube exposes one-request authorization and removes it when the platform changes', () => {
  const form = downloadForm({url:'https://www.youtube.com/watch?v=abcdefghijk'});
  Object.assign(form.context, {filenameEdited:false, filenameRequest:null, mediaRequest:null, setTimeout() { return 1; }});
  vm.runInContext(source.slice(source.indexOf('function suggestName('), source.indexOf('function videoPlatform(')), form.context);
  form.context.suggestName();
  assert.equal(form.field('#platform-auth').hidden, false);
  assert.equal(form.field('#youtube-cookie-help').hidden, false);
  assert.match(form.field('#platform-cookie-label').textContent, /YouTube/);
  form.field('#platform-cookie').value = 'SAPISID=test-session-secret';
  form.field('#download-url').value = 'https://files.example.org/file.zip';
  form.context.suggestName();
  assert.equal(form.field('#platform-cookie').value, '');
  assert.equal(form.field('#platform-auth').hidden, true);
  assert.equal(form.field('#youtube-cookie-help').hidden, true);
});

test('YouTube Cookie is only passed to this extraction and cleared on success and failure', async () => {
  for (const failure of [false, true]) {
    const form = downloadForm({url:'https://www.youtube.com/watch?v=abcdefghijk'});
    Object.assign(form.context, {mediaRequest:null, escape:value => value, applyMediaFormat() {}});
    form.field('#platform-cookie').value = 'SAPISID=test-session-secret';
    form.context.api.resolveMedia = async (url, cookie) => {
      form.calls.push(['resolveMedia', url, cookie]);
      assert.equal(form.field('#platform-cookie').value, '');
      if (failure) throw new Error('YouTube 仍要求验证当前访问会话');
      return {id:'authorized-preview', title:'Authorized video', platform:'YouTube', formats:[{id:'format-1', label:'720p', needs_merge:false}], ffmpeg_available:true};
    };
    vm.runInContext(source.slice(source.indexOf('async function parseVideo('), source.indexOf("$('#parse-video').addEventListener")), form.context);
    await form.context.parseVideo();
    assert.equal(form.calls[0][2], 'SAPISID=test-session-secret');
    assert.equal(form.field('#platform-cookie').value, '');
    if (failure) {
      assert.match(form.field('#form-error').textContent, /验证/);
    } else {
      await form.context.submit({preventDefault() {}});
      assert.equal(form.calls[1][0], 'createMedia');
      assert.equal('cookie' in form.calls[1][1], false);
    }
  }
});

function authorizationForm() {
  const form = downloadForm({url:'https://files.example.org/private.zip'});
  Object.assign(form.context, {
    authorizationTarget:null, authorizationVersion:0, authorizing:false,
    pending:new Set(), render() {}, snapshot:{tasks:[{id:'task-1', url:'https://files.example.org/private.zip'}]},
  });
  form.context.api.resume = async (id, body) => { form.calls.push(['resume', id, body]); };
  vm.runInContext(source.slice(source.indexOf('function openAuthorizationDialog('), source.indexOf('async function runAction(')), form.context);
  form.context.openAuthorizationDialog('task-1');
  return form;
}

test('authorization dialog resumes the existing task and clears its password field on success or close', async () => {
  const form = authorizationForm();
  assert.equal(form.field('#authorization-origin').textContent, 'https://files.example.org');
  form.field('#authorization-cookie').value = 'session=test-only';
  await form.field('#authorization-form').listeners.get('submit')({preventDefault() {}});
  assert.equal(form.calls[0][0], 'resume');
  assert.equal(form.calls[0][1], 'task-1');
  assert.equal(form.calls[0][2].cookie, 'session=test-only');
  assert.equal(form.field('#authorization-cookie').value, '');
  assert.equal(form.context.authorizationTarget, null);

  form.context.openAuthorizationDialog('task-1');
  form.field('#authorization-cookie').value = 'session=test-only';
  form.field('#authorization-dialog').close();
  assert.equal(form.field('#authorization-cookie').value, '');
  assert.equal(form.context.authorizationTarget, null);
});

test('authorization dialog refuses to forward a Cookie if the task URL changed', async () => {
  const form = authorizationForm();
  form.field('#authorization-cookie').value = 'session=test-only';
  form.context.snapshot.tasks[0].url = 'https://other.example.org/private.zip';
  await form.field('#authorization-form').listeners.get('submit')({preventDefault() {}});
  assert.equal(form.calls.length, 0);
  assert.match(form.field('#authorization-error').textContent, /链接已改变/);
});

test('existing in-memory authorization can resume directly; missing authorization and 401 offer the dialog', () => {
  const context = vm.createContext({});
  vm.runInContext(source.slice(source.indexOf('function authorizationNeeded('), source.indexOf('function actionButton(')), context);
  assert.equal(context.authorizationNeeded({state:'paused', authorization_required:true, authorization_needed:false}), false);
  assert.equal(context.authorizationNeeded({state:'paused', authorization_required:true, authorization_needed:true}), true);
  assert.equal(context.authorizationNeeded({state:'error', error:'HTTP 401', authorization_needed:false}), true);
  assert.equal(context.authorizationNeeded({state:'error', error:'HTTP 401', media:{platform:'X'}}), false);
});

test('401 and 403 file failures offer authorization while media retains reparse', () => {
  const context = vm.createContext({pending:new Set(), connected:true, icon:() => '', escape:value => String(value)});
  vm.runInContext(source.slice(source.indexOf('function authorizationNeeded('), source.indexOf('function row(')), context);
  const task = {id:'task-1', state:'error', filename:'private.zip', error:'HTTP 403', authorization_needed:false};
  assert.match(context.actionButton({...task, authorization_required:true}), /data-action="authorize"/);
  assert.match(context.actionButton({...task, authorization_required:false}), /data-action="authorize"/);
  assert.match(context.actionButton({...task, error:'HTTP 404', authorization_required:false}), /data-action="resume"/);
  assert.match(context.actionButton({...task, authorization_required:true, media:{platform:'X'}}), /data-action="reparse"/);
});

test('resume adapters preserve the old body-free call and pass only explicit authorization', async () => {
  const apiSource = fs.readFileSync(path.join(__dirname, '../web/api.js'), 'utf8').replace(/^export /gm, '');
  const requests = [];
  const web = vm.createContext({window:{}, AbortSignal, fetch:async (url, options) => {
    requests.push({url, options});
    return {ok:true, json:async () => ({})};
  }});
  vm.runInContext(`${apiSource}\nglobalThis.testApi = api;`, web);
  await web.testApi.resume('task-1');
  await web.testApi.resume('task-1', {cookie:'session=test-only'});
  assert.equal(requests[0].options.body, undefined);
  assert.deepEqual(JSON.parse(requests[1].options.body), {cookie:'session=test-only'});

  const calls = [];
  const desktop = vm.createContext({window:{__TAURI__:{core:{invoke:async (command, args) => calls.push({command,args})}}}});
  vm.runInContext(`${apiSource}\nglobalThis.testApi = api;`, desktop);
  await desktop.testApi.resume('task-1');
  await desktop.testApi.resume('task-1', {cookie:'session=test-only'});
  assert.equal(calls[0].command, 'desktop_resume');
  assert.equal('cookie' in calls[0].args, false);
  assert.equal(calls[1].args.cookie, 'session=test-only');
});

test('custom headers preserve values after the first colon and allow header-only file authorization', async () => {
  const form = downloadForm({url:'https://files.example.org/private.zip'});
  form.context.filenameSuggestion = null;
  form.field('#download-headers').value = '\r\nAuthorization: Bearer test-only\r\nReferer: https://example.org/watch?id=1:2\nUser-Agent: Download Test/1.0\nX-API-Key: test-key\n';
  await form.context.submit({preventDefault() {}});
  assert.equal(form.calls.length, 1, 'Header-only requests skip anonymous filename lookup');
  assert.equal(form.calls[0][0], 'create');
  assert.equal('cookie' in form.calls[0][1], false);
  assert.deepEqual(JSON.parse(JSON.stringify(form.calls[0][1].headers)), [
    ['Authorization', 'Bearer test-only'],
    ['Referer', 'https://example.org/watch?id=1:2'],
    ['User-Agent', 'Download Test/1.0'],
    ['X-API-Key', 'test-key'],
  ]);
  assert.equal(form.field('#download-headers').value, '');
});

test('Cookie shortcuts combine with distinct headers but reject duplicate Cookie fields', () => {
  const {authorizationForDownload} = parser();
  const raw = 'https://files.example.org/private.zip';
  assert.deepEqual(JSON.parse(JSON.stringify(authorizationForDownload(raw, 'session=test-only', 'Authorization: Bearer test-only'))), {
    cookie:'session=test-only', headers:[['Authorization', 'Bearer test-only']],
  });
  assert.deepEqual(JSON.parse(JSON.stringify(authorizationForDownload(raw, '', 'Cookie: session=test-only'))), {
    headers:[['Cookie', 'session=test-only']],
  });
  assert.throws(() => authorizationForDownload(raw, 'session=test-only', 'cOoKiE: other=test-only'), /不能同时填写/);
  assert.throws(() => authorizationForDownload(raw, '', 'Authorization: Bearer test-only\nauthorization: Bearer other'), /重复/);
  assert.equal(authorizationForDownload(raw, '', '\n  \n'), undefined);
});

test('custom headers reject download-control fields, malformed lines, control bytes and non-ASCII values without echoing credentials', () => {
  const {authorizationForDownload} = parser();
  const raw = 'https://files.example.org/private.zip';
  const forbidden = ['Host', 'Range', 'If-Range', 'If-Match', 'If-None-Match', 'If-Modified-Since', 'If-Unmodified-Since', 'Content-Length', 'Transfer-Encoding', 'Connection', 'Keep-Alive', 'TE', 'Trailer', 'Upgrade', 'Proxy-Authorization', 'Proxy-Connection', 'Accept-Encoding'];
  for (const name of forbidden) assert.throws(() => authorizationForDownload(raw, '', `${name}: private-test-value`), /由下载器管理/);
  for (const line of [
    ':authority: private-test-value',
    'Authorization private-test-value',
    'Invalid Name: private-test-value',
    'X-授权: private-test-value',
    'Authorization: private-test-value\rX-Injected: 1',
    'Authorization: private-test-value\t',
    'Authorization: private-test-value\0',
    'Authorization: private-test-value\x7f',
    'Authorization: private-test-value中文',
    'Authorization: private-test-value\ninvalid-second-line',
  ]) {
    let error;
    try { authorizationForDownload(raw, '', line); } catch (caught) { error = caught; }
    assert.ok(error, 'invalid header input must be rejected');
    assert.equal(error.message.includes('private-test-value'), false);
  }
  assert.throws(() => authorizationForDownload(raw, 'session=test-only\r\nX-Injected: 1', ''), /单行值/);
  assert.throws(() => authorizationForDownload(raw, 'session=中文', ''), /ASCII/);
});

test('custom authorization enforces HTTPS and combined header count and byte limits', () => {
  const {authorizationForDownload} = parser();
  for (const raw of ['http://files.example.org/private.zip', 'https://user:pass@files.example.org/private.zip', 'not a URL']) {
    assert.throws(() => authorizationForDownload(raw, '', 'X-API-Key: test-key'), /HTTPS/);
  }
  const raw = 'https://files.example.org/private.zip';
  const headers = Array.from({length:48}, (_, i) => `X-Test-${i}: 1`).join('\n');
  assert.equal(authorizationForDownload(raw, '', headers).headers.length, 48);
  assert.throws(() => authorizationForDownload(raw, 'session=test-only', headers), /48 条/);
  assert.equal(authorizationForDownload(raw, '', `X: ${'a'.repeat(16383)}`).headers[0][1].length, 16383);
  assert.throws(() => authorizationForDownload(raw, '', `X: ${'a'.repeat(16384)}`), /16 KiB/);
  assert.throws(() => authorizationForDownload(raw, 'a'.repeat(16379), ''), /16 KiB/);
  assert.throws(() => authorizationForDownload(raw, 'a'.repeat(16377), 'X: 1'), /16 KiB/);
});

test('editing authorization invalidates scheduled anonymous filename discovery and its suggestion', async () => {
  const form = downloadForm({url:'https://files.example.org/private.zip'});
  form.context.filenameRequest = {version:1};
  form.field('#download-filename').value = 'anonymous-name.zip';
  form.field('#download-headers').value = 'X-API-Key: test-key';
  let timerCancelled = false;
  form.context.clearTimeout = () => { timerCancelled = true; };
  form.context.downloadAuthorizationChanged();
  assert.equal(timerCancelled, true);
  assert.equal(form.context.filenameVersion, 2);
  assert.equal(form.context.filenameSuggestion, null);
  assert.equal(form.context.filenameRequest, null);
  assert.equal(form.field('#download-filename').value, '');
  await form.context.submit({preventDefault() {}});
  assert.equal(form.calls.length, 1);
  assert.equal(form.calls[0][0], 'create');
});

test('headers are cleared when switching to media mode and never reach media extraction or task APIs', async () => {
  const form = downloadForm({url:'https://files.example.org/private.zip'});
  Object.assign(form.context, {filenameRequest:null, mediaRequest:null, setTimeout() { return 1; }});
  vm.runInContext(source.slice(source.indexOf('function suggestName('), source.indexOf('function videoPlatform(')), form.context);
  form.field('#download-headers').value = 'Authorization: Bearer test-only';
  form.field('#download-cookie').value = 'session=test-only';
  form.field('#download-mode').value = 'media';
  form.context.suggestName();
  assert.equal(form.field('#download-headers').value, '');
  assert.equal(form.field('#download-cookie').value, '');
  assert.equal(form.field('#download-authorization').hidden, true);
  await form.context.submit({preventDefault() {}});
  assert.deepEqual(form.calls, [['resolveMedia']]);
});

test('header validation prevents API calls and authorized failures hide echoed header secrets', async () => {
  for (const [url, headers, pattern] of [
    ['http://files.example.org/private.zip', 'Authorization: Bearer test-only', /HTTPS/],
    ['https://files.example.org/private.zip', 'Host: private-test-value', /由下载器管理/],
  ]) {
    const form = downloadForm({url});
    form.field('#download-headers').value = headers;
    await form.context.submit({preventDefault() {}});
    assert.equal(form.calls.length, 0);
    assert.match(form.field('#form-error').textContent, pattern);
    assert.equal(form.field('#form-error').textContent.includes('private-test-value'), false);
  }
  const form = downloadForm({url:'https://files.example.org/private.zip'});
  form.field('#download-headers').value = 'Authorization: Bearer private-test-value';
  form.context.api.create = async () => { throw new Error('Authorization: Bearer private-test-value'); };
  await form.context.submit({preventDefault() {}});
  assert.match(form.field('#form-error').textContent, /授权下载未能启动/);
  assert.equal(form.field('#form-error').textContent.includes('private-test-value'), false);
});

test('retry accepts replacement headers without a Cookie and clears them on success, close and target change', async () => {
  const form = authorizationForm();
  form.field('#authorization-headers').value = 'Authorization: Bearer old-test-value';
  form.context.api.resume = async (id, body) => {
    form.calls.push(['resume', id, body]);
    if (form.calls.length === 1) throw new Error('server echoed old-test-value');
  };
  const submit = form.field('#authorization-form').listeners.get('submit');
  await submit({preventDefault() {}});
  assert.equal(form.field('#authorization-error').textContent.includes('old-test-value'), false);
  form.field('#authorization-headers').value = 'Authorization: Bearer new-test-value\nUser-Agent: Download Test/1.0';
  await submit({preventDefault() {}});
  assert.deepEqual(JSON.parse(JSON.stringify(form.calls[1])), ['resume', 'task-1', {headers:[['Authorization', 'Bearer new-test-value'], ['User-Agent', 'Download Test/1.0']]}]);
  assert.equal(form.field('#authorization-headers').value, '');
  form.context.openAuthorizationDialog('task-1');
  form.field('#authorization-headers').value = 'X-API-Key: test-key';
  form.context.snapshot.tasks.push({id:'task-2', url:'https://other.example.org/private.zip'});
  form.context.openAuthorizationDialog('task-2');
  assert.equal(form.field('#authorization-headers').value, '');
  form.field('#authorization-headers').value = 'X-API-Key: test-key';
  form.field('#authorization-dialog').close();
  assert.equal(form.field('#authorization-headers').value, '');
});

test('retry requires at least one authorization field, prevents changed origins and rejects invalid headers', async () => {
  const empty = authorizationForm();
  await empty.field('#authorization-form').listeners.get('submit')({preventDefault() {}});
  assert.equal(empty.calls.length, 0);
  assert.match(empty.field('#authorization-error').textContent, /Cookie 或请求 Header/);
  const changed = authorizationForm();
  changed.field('#authorization-headers').value = 'X-API-Key: test-key';
  changed.context.snapshot.tasks[0].url = 'https://other.example.org/private.zip';
  await changed.field('#authorization-form').listeners.get('submit')({preventDefault() {}});
  assert.equal(changed.calls.length, 0);
  assert.match(changed.field('#authorization-error').textContent, /链接已改变/);
  const invalid = authorizationForm();
  invalid.field('#authorization-headers').value = 'Range: private-test-value';
  await invalid.field('#authorization-form').listeners.get('submit')({preventDefault() {}});
  assert.equal(invalid.calls.length, 0);
  assert.equal(invalid.field('#authorization-error').textContent.includes('private-test-value'), false);
});

test('Web and Tauri create/resume adapters send custom headers in payloads, never localhost transport headers', async () => {
  const apiSource = fs.readFileSync(path.join(__dirname, '../web/api.js'), 'utf8').replace(/^export /gm, '');
  const authorization = {headers:[['Authorization', 'Bearer test-only'], ['User-Agent', 'Download Test/1.0']]};
  const requests = [];
  const web = vm.createContext({window:{}, AbortSignal, fetch:async (url, options) => {
    requests.push({url, options}); return {ok:true, json:async () => ({})};
  }});
  vm.runInContext(`${apiSource}\nglobalThis.testApi = api;`, web);
  await web.testApi.create({url:'https://files.example.org/private.zip', ...authorization});
  await web.testApi.resume('task-1', authorization);
  assert.deepEqual(JSON.parse(requests[0].options.body).headers, authorization.headers);
  assert.deepEqual(JSON.parse(requests[1].options.body), authorization);
  for (const {options} of requests) assert.deepEqual(JSON.parse(JSON.stringify(options.headers)), {'Content-Type':'application/json', 'X-FFDM-Client':'local-ui'});
  const calls = [];
  const desktop = vm.createContext({window:{__TAURI__:{core:{invoke:async (command, args) => calls.push({command,args})}}}});
  vm.runInContext(`${apiSource}\nglobalThis.testApi = api;`, desktop);
  await desktop.testApi.create({url:'https://files.example.org/private.zip', ...authorization});
  await desktop.testApi.resume('task-1', authorization);
  assert.equal(calls[0].command, 'desktop_create');
  assert.deepEqual(calls[0].args.task.headers, authorization.headers);
  assert.equal(calls[1].command, 'desktop_resume');
  assert.deepEqual(calls[1].args.headers, authorization.headers);
  assert.equal(calls[1].args.cookie, undefined);
});

test('paused and failed HTTPS files expose Header editing in task details', () => {
  for (const [state, url, media, expected] of [
    ['paused', 'https://files.example.org/private.zip', null, true],
    ['error', 'https://files.example.org/private.zip', null, true],
    ['error', 'http://files.example.org/private.zip', null, false],
    ['completed', 'https://files.example.org/private.zip', null, false],
    ['error', 'https://files.example.org/private.zip', {platform:'X'}, false],
  ]) {
    const form = downloadForm({url});
    const task = {id:'task-1', filename:'file.zip', url, state, media, created_at:0, connections:4};
    Object.assign(form.context, {
      snapshot:{tasks:[task], download_directory:'/downloads'}, selectedId:'task-1', pending:new Set(),
      stateLabels:{}, escape:value => String(value), bytes:() => '0 B', displayUrl:() => url,
      active:() => false, icon:() => '', actionButton:() => '',
      replacePreservingFocus:(el, html) => { el.innerHTML = html; },
    });
    vm.runInContext(source.slice(source.indexOf('function renderDetail('), source.indexOf("document.addEventListener('keydown'")), form.context);
    form.context.renderDetail();
    assert.equal(form.field('#detail-actions').innerHTML.includes('编辑请求 Header'), expected, `${state} ${url} ${!!media}`);
  }
});
