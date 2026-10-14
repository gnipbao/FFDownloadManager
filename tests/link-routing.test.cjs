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
    filenameVersion:1, filenameTimer:null, filenameSuggestion:{extension:'zip'},
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
  field('#platform-cookie').value = 'platform=test-only';
  context.suggestName();
  assert.equal(context.mediaSite, null);
  assert.equal(context.mediaPreview, null);
  assert.equal(field('#media-source').hidden, true);
  assert.equal(field('#media-panel').hidden, true);
  assert.equal(context.filenameTimer, 1);
  assert.equal(field('#download-cookie').value, '');
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
  await media.context.submit({preventDefault() {}});
  assert.equal(media.calls[0][0], 'createMedia');
  assert.equal('cookie' in media.calls[0][1], false);
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

test('closing new-download or changing the URL removes both authorization inputs', () => {
  const form = downloadForm({url:'https://files.example.org/private.zip'});
  form.field('#download-cookie').value = 'session=test-only';
  form.field('#platform-cookie').value = 'platform=test-only';
  form.field('#new-dialog').close();
  assert.equal(form.field('#download-cookie').value, '');
  assert.equal(form.field('#platform-cookie').value, '');

  Object.assign(form.context, {filenameEdited:false, filenameRequest:null, mediaRequest:null, setTimeout() { return 1; }});
  vm.runInContext(source.slice(source.indexOf('function suggestName('), source.indexOf('function videoPlatform(')), form.context);
  form.field('#download-cookie').value = 'session=test-only';
  form.field('#download-url').value = 'https://other.example.org/private.zip';
  form.context.suggestName();
  assert.equal(form.field('#download-cookie').value, '');
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

test('expired authorization can be replaced after 403 without changing anonymous or media retry actions', () => {
  const context = vm.createContext({pending:new Set(), connected:true, icon:() => '', escape:value => String(value)});
  vm.runInContext(source.slice(source.indexOf('function authorizationNeeded('), source.indexOf('function row(')), context);
  const task = {id:'task-1', state:'error', filename:'private.zip', error:'HTTP 403', authorization_needed:false};
  assert.match(context.actionButton({...task, authorization_required:true}), /data-action="authorize"/);
  assert.match(context.actionButton({...task, authorization_required:false}), /data-action="resume"/);
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
