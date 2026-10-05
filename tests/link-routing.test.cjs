const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../web/app.js'), 'utf8');
const routing = source.slice(source.indexOf('function videoPlatform('), source.indexOf('function updateSubmitButton('));
const sharedCases = JSON.parse(fs.readFileSync(path.join(__dirname, 'fixtures/link-routing.json'), 'utf8'));
const labels = {'Qishui Music':'汽水音乐', Douyin:'抖音', Xiaohongshu:'小红书', 'WeChat Channels':'视频号'};
function parser() {
  const context = vm.createContext({URL});
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
  assert.equal(mediaPlatform('https://ton.x.com/archive-example.zip', 'media'), '音视频');
  assert.equal(mediaPlatform('https://media.example.org/watch/abc', 'media'), '音视频');
  assert.equal(mediaPlatform('https://music.douyin.com/unknown-page', 'media'), '音视频');
  assert.equal(mediaPlatform('http://music.douyin.com/qishui/share/track?track_id=1234567890123456789', 'media'), '音视频');
});

function downloadForm({url, mode = 'auto', preview = null}) {
  const context = parser();
  const fields = new Map();
  const calls = [];
  const field = selector => {
    if (!fields.has(selector)) fields.set(selector, {
      value:'', open:true, textContent:'', hidden:false,
      addEventListener(_event, callback) { context.submit = callback; },
      close() { this.open = false; },
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
  vm.runInContext(source.slice(source.indexOf("$('#new-form').addEventListener('submit'"), source.indexOf('\nfunction setFilter(')), context);
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
});

test('switching to file mode clears stale media previews before filename discovery', () => {
  const {context, field} = downloadForm({url:'https://x.com/user/status/1234567890', preview:{id:'stale'}});
  Object.assign(context, {filenameEdited:false, filenameRequest:null, mediaRequest:null, setTimeout() { return 1; }});
  vm.runInContext(source.slice(source.indexOf('function suggestName('), source.indexOf('function videoPlatform(')), context);
  field('#download-mode').value = 'direct';
  context.suggestName();
  assert.equal(context.mediaSite, null);
  assert.equal(context.mediaPreview, null);
  assert.equal(field('#media-source').hidden, true);
  assert.equal(field('#media-panel').hidden, true);
  assert.equal(context.filenameTimer, 1);
});
