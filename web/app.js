import { api, isDesktop, onDesktopEvent } from './api.js';
import { initWorkbench } from './workbench.js';

const $ = (selector) => document.querySelector(selector);
const icons = {
  grid: '<rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/>',
  download: '<path d="M12 3v12m-5-5 5 5 5-5M4 16v4a1 1 0 0 0 1 1h14a1 1 0 0 0 1-1v-4"/>',
  pause: '<path d="M8 5v14M16 5v14"/>',
  'check-circle': '<circle cx="12" cy="12" r="9"/><path d="m8 12 3 3 5-6"/>',
  alert: '<circle cx="12" cy="12" r="9"/><path d="M12 7v6m0 3v.2"/>',
  spark: '<path d="m12 2 2.8 7.2L22 12l-7.2 2.8L12 22l-2.8-7.2L2 12l7.2-2.8z"/>',
  'arrow-right': '<path d="M4 12h16m-6-6 6 6-6 6"/>',
  folder: '<path d="M3 6a2 2 0 0 1 2-2h5l2 3h7a2 2 0 0 1 2 2v10a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1z"/>',
  external: '<path d="M14 3h7v7m0-7L11 13m-1-9H5a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2h13a2 2 0 0 0 2-2v-5"/>',
  home: '<path d="m3 10 9-7 9 7v10H3zM9 20v-7h6v7"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  search: '<circle cx="10.5" cy="10.5" r="6.5"/><path d="m16 16 5 5"/>',
  play: '<path d="m8 4 12 8-12 8z"/>',
  shield: '<path d="m12 3 8 3v6c0 4-4 7-8 9-4-2-8-5-8-9V6z"/><path d="m8 12 3 3 5-6"/>',
  x: '<path d="m6 6 12 12M6 18 18 6"/>',
  'chevron-down': '<path d="m6 9 6 6 6-6"/>',
  file: '<path d="M14 2H5v20h14V7zM14 2v5h5M8 12h8M8 16h5"/>',
  more: '<circle cx="5" cy="12" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/>',
  retry: '<path d="M3 4v6h6m-6 0a9 9 0 1 1 1 8"/>',
  trash: '<path d="M3 6h18M9 6V3h6v3M5 6l1 15h12l1-15M10 10v7m4-7v7"/>',
  copy: '<rect x="8" y="8" width="13" height="13" rx="2"/><path d="M16 8V3H3v13h5"/>',
};
const icon = (name) => `<svg class="icon" viewBox="0 0 24 24" aria-hidden="true">${icons[name] || icons.file}</svg>`;
document.querySelectorAll('[data-icon]').forEach((el) => { el.innerHTML = icon(el.dataset.icon); });
const escape = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&':'&amp;', '<':'&lt;', '>':'&gt;', '"':'&quot;', "'":'&#39;' })[c]);
const active = (task) => ['queued', 'connecting', 'downloading', 'pausing', 'verifying', 'merging'].includes(task.state);
const stateLabels = {queued:'排队中', connecting:'正在连接', downloading:'下载中', pausing:'正在保存进度', paused:'已暂停', verifying:'正在校验文件', merging:'正在合并音视频', completed:'已完成', error:'下载失败'};
const filterLabels = {all:'全部下载', active:'正在下载', paused:'已暂停', completed:'已完成', error:'需要处理'};
let snapshot = {tasks:[], download_directory:'', can_reveal:false, max_active:2};
let desktopPlatform = 'macos';
let connected = false;
let filter = 'all';
let selectedId = null;
let samples = [];
let refreshing = null;
const pending = new Set();
let filenameVersion = 0;
let filenameTimer = null;
let filenameSuggestion = null;
let filenameEdited = false;
let filenameRequest = null;
let mediaPreview = null;
let mediaRequest = null;
let mediaSite = null;
let submitting = false;

function bytes(n) {
  if (n === null || n === undefined) return '待获取';
  if (n < 1024) return `${n} B`;
  const units = ['KiB','MiB','GiB','TiB'];
  let i = -1;
  do { n /= 1024; i++; } while (n >= 1024 && i < units.length - 1);
  return `${n >= 100 ? n.toFixed(0) : n.toFixed(1)} ${units[i]}`;
}
function speed(n) {
  if (n === 0) return ['0', 'MB/s'];
  if (n < 1e6) return [(n / 1000).toFixed(n < 1e5 ? 1 : 0), 'KB/s'];
  return [(n / 1e6).toFixed(n < 1e8 ? 2 : 1), 'MB/s'];
}
function eta(seconds) {
  if (seconds == null) return '';
  if (seconds < 60) return `剩余 ${seconds} 秒`;
  if (seconds < 3600) return `剩余 ${Math.ceil(seconds / 60)} 分钟`;
  return `剩余 ${(seconds / 3600).toFixed(1)} 小时`;
}
function hostname(task) {
  if (task.demo) return '本机回环 · 不代表公网速度';
  try { return new URL(task.url).hostname; } catch { return '未知来源'; }
}
function displayUrl(task) {
  try { const url = new URL(task.url); return `${url.origin}${url.pathname}${url.search ? '?…' : ''}`; } catch { return task.url; }
}
function percentage(task) { return task.total_bytes ? Math.min(100, task.completed_bytes / task.total_bytes * 100) : 0; }
function toast(message, error = false) {
  const node = document.createElement('div');
  node.className = `toast${error ? ' error' : ''}`;
  node.textContent = message;
  $('#toast-container').append(node);
  setTimeout(() => node.remove(), error ? 7000 : 4200);
}
function replacePreservingFocus(element, html) {
  const key = element.contains(document.activeElement) ? document.activeElement.dataset.focusKey : null;
  if (element.innerHTML !== html) element.innerHTML = html;
  if (key) [...element.querySelectorAll('[data-focus-key]')].find((el) => el.dataset.focusKey === key)?.focus({preventScroll:true});
}
function chart() {
  const now = Date.now();
  samples = samples.filter((p) => p.time >= now - 60000);
  const peak = Math.max(1, ...samples.map((p) => p.value));
  const points = samples.map((p) => [Math.max(0, (p.time - now + 60000) / 100), 77 - p.value / peak * 66]);
  const path = points.length ? points.map(([x,y],i) => `${i ? 'L' : 'M'}${x.toFixed(1)} ${y.toFixed(1)}`).join(' ') : 'M0 77 H600';
  $('#chart-line').setAttribute('d', path);
  $('#chart-area').setAttribute('d', points.length ? `${path} L${points.at(-1)[0]} 80 L${points[0][0]} 80 Z` : '');
  const value = speed(peak === 1 ? 0 : peak);
  $('#chart-scale').textContent = `峰值 ${value.join(' ')}`;
}
function renderOverview() {
  const tasks = snapshot.tasks;
  const running = tasks.filter(active);
  const completed = tasks.filter((t) => t.state === 'completed');
  const currentSpeed = connected ? tasks.reduce((n,t) => n + t.speed_bytes, 0) : 0;
  const [number, unit] = speed(currentSpeed);
  $('#speed-number').textContent = number;
  $('#speed-unit').textContent = unit;
  $('.speed-value').title = `${(currentSpeed * 8 / 1e6).toFixed(2)} Mbps`;
  const connections = tasks.reduce((n,t) => n + t.active_connections, 0);
  $('#speed-caption').textContent = !connected ? '服务未连接，正在自动重试…' : running.length ? `${running.length} 个进行中任务 · ${connections} 个活跃连接${running.some(t => t.demo) ? ' · 含本地体验任务' : ''}` : '内核已就绪，随时开始下一次下载';
  $('#stat-active').textContent = running.length;
  const queued = tasks.filter((t) => t.state === 'queued').length;
  $('#stat-queued').textContent = queued ? `${queued} 项排队 · 最多同时下载 ${snapshot.max_active} 项` : running.length ? '下载正在这台电脑上进行' : '等待新的下载';
  $('#stat-completed').textContent = completed.length;
  $('#stat-size').textContent = `已下载 ${bytes(completed.reduce((n,t) => n + t.completed_bytes, 0))}`;
  $('#pause-all').disabled = !connected || !running.some(t => !['pausing','verifying'].includes(t.state)) || pending.has('all');
  $('#sidebar-folder').disabled = !connected || !snapshot.can_reveal;
  $('#connection-pill').classList.toggle('offline', !connected);
  $('#connection-pill span').textContent = connected ? 'Rust 内核已连接' : '本地服务未连接';
  $('#save-location').textContent = snapshot.download_directory || '等待本地服务连接';
  $('#save-location').title = snapshot.download_directory;
  $('#list-footer-text').textContent = snapshot.download_directory ? `保存到 ${snapshot.download_directory}` : '文件直接保存到你的电脑';
  $('#list-footer-text').title = snapshot.download_directory;
  for (const type of Object.keys(filterLabels)) {
    $(`#count-${type}`).textContent = type === 'all' ? tasks.length : type === 'active' ? running.length : tasks.filter((t) => t.state === type).length;
  }
  chart();
}
function actionButton(task) {
  let action, name, label;
  if (['queued','connecting','downloading','merging'].includes(task.state)) { action = 'pause'; name = 'pause'; label = '暂停下载'; }
  else if (task.state === 'paused') { action = 'resume'; name = 'play'; label = '继续下载'; }
  else if (task.state === 'error') {
    const reparse = task.media && /\b(?:403|410)\b|媒体地址已失效/.test(task.error || '');
    action = reparse ? 'reparse' : 'resume'; name = 'retry'; label = reparse ? '重新解析视频' : '重试下载';
  }
  else if (task.state === 'completed') {
    if (!snapshot.can_reveal) return `<a class="icon-button active-action" href="/api/tasks/${encodeURIComponent(task.id)}/file" download="${escape(task.filename)}" title="保存副本" aria-label="保存副本">${icon('download')}</a>`;
    action = 'reveal'; name = 'folder'; label = desktopPlatform === 'windows' ? '在资源管理器中显示' : '在 Finder 中显示';
  } else { return `<button class="icon-button active-action" disabled title="正在保存或校验文件">${icon('pause')}</button>`; }
  return `<button class="icon-button active-action" data-action="${action}" data-id="${escape(task.id)}" data-focus-key="${escape(task.id)}-${action}" title="${label}" aria-label="${label}：${escape(task.filename)}" ${pending.has(task.id) || !connected ? 'disabled' : ''}>${icon(name)}</button>`;
}
function row(task) {
  const ext = hasExtension(task.filename) ? task.filename.split('.').at(-1).toUpperCase().slice(0,7) : 'FILE';
  const kind = task.demo ? 'demo' : /^(ZIP|RAR|GZ|XZ|7Z|DMG|TAR)$/.test(ext) ? 'archive' : /^(MP4|MOV|MKV|WEBM)$/.test(ext) ? 'video' : '';
  const progress = percentage(task);
  const speedText = task.speed_bytes && connected ? speed(task.speed_bytes).join(' ') : '—';
  return `<article class="task-row" data-task-id="${escape(task.id)}">
    <div class="task-file"><div class="file-type ${kind}">${icon('file')}<span>${escape(ext)}</span></div><div class="file-info"><button class="file-name" data-action="detail" data-id="${escape(task.id)}" data-focus-key="${escape(task.id)}-name" title="${escape(task.filename)}">${escape(task.filename)}</button><div class="file-meta">${task.demo ? '<b class="demo-tag">本地体验</b>' : ''}<span title="${escape(hostname(task))}">${escape(hostname(task))}</span></div></div></div>
    <div class="task-size">${bytes(task.total_bytes)}</div>
    <div class="task-progress"><div class="progress-label"><span class="${task.state}">${stateLabels[task.state] || task.state}</span><span>${task.state === 'completed' ? '100%' : task.total_bytes ? `${progress.toFixed(1)}%` : '—'}</span></div><div class="progress-track ${task.state}"><i data-progress="${progress.toFixed(2)}"></i></div></div>
    <div class="task-speed">${speedText}<small>${task.state === 'downloading' ? eta(task.eta_seconds) : task.state === 'completed' ? '保存在本机' : bytes(task.completed_bytes)}</small></div>
    <div class="row-actions">${actionButton(task)}<button class="icon-button" data-action="detail" data-id="${escape(task.id)}" data-focus-key="${escape(task.id)}-detail" title="任务详情" aria-label="任务详情：${escape(task.filename)}">${icon('more')}</button></div>
  </article>`;
}
function renderTasks() {
  let tasks = snapshot.tasks.filter((t) => filter === 'all' || (filter === 'active' ? active(t) : t.state === filter));
  const query = $('#search').value.toLowerCase().trim();
  if (query) tasks = tasks.filter((t) => `${t.filename} ${hostname(t)}`.toLowerCase().includes(query));
  if ($('#sort').value === 'old') tasks.sort((a,b) => a.created_at - b.created_at);
  if ($('#sort').value === 'name') tasks.sort((a,b) => a.filename.localeCompare(b.filename, 'zh-CN'));
  $('#list-title').textContent = filterLabels[filter];
  $('#list-count').textContent = tasks.length;
  replacePreservingFocus($('#task-list'), tasks.map(row).join(''));
  $('#task-list').querySelectorAll('[data-progress]').forEach((el) => { el.style.width = `${el.dataset.progress}%`; });
  $('#empty-state').hidden = tasks.length > 0;
  const initial = !snapshot.tasks.length && !query;
  $('#empty-title').textContent = query ? '没有找到匹配的文件' : initial ? '暂无下载任务' : `还没有${filterLabels[filter]}的任务`;
  $('#empty-description').textContent = query ? '换一个文件名或下载来源试试。' : initial ? '添加下载链接，文件将保存在本机。' : '切换左侧分类，或添加一个新的下载。';
  $('#empty-actions').hidden = !initial;
  $('#empty-actions').style.display = initial ? '' : 'none';
  if ($('#detail-dialog').open) renderDetail();
}
function render() { renderOverview(); renderTasks(); }

async function refresh() {
  if (refreshing) return refreshing;
  refreshing = (async () => {
    try {
      const previous = new Map(snapshot.tasks.map((t) => [t.id,t.state]));
      snapshot = await api.snapshot();
      connected = true;
      for (const task of snapshot.tasks) {
        if (previous.has(task.id) && previous.get(task.id) !== 'completed' && task.state === 'completed') toast(`下载完成 · ${task.filename}`);
      }
      const time = Date.now();
      const value = snapshot.tasks.reduce((n,t) => n + t.speed_bytes, 0);
      if (!samples.length || time - samples.at(-1).time >= 800) samples.push({time,value});
    } catch {
      connected = false;
    }
    render();
  })();
  try { await refreshing; } finally { refreshing = null; }
}
async function poll() { await refresh(); setTimeout(poll, 1000); }

function newDialog(url = '') {
  $('#new-form').reset();
  filenameEdited = false;
  $('#form-error').hidden = true;
  $('#download-url').value = url;
  $('#new-dialog').showModal();
  $('#download-url').focus();
  suggestName();
}
function hasExtension(name) {
  const match = name.match(/^.+\.([a-z0-9]{1,12})$/i);
  return !!match && /[a-z]/i.test(match[1]);
}
function completeFilenameInput() {
  const field = $('#download-filename');
  const name = field.value.trim();
  if (name && !hasExtension(name) && filenameSuggestion?.extension) {
    field.value = `${name}.${filenameSuggestion.extension}`;
  }
}
async function resolveFilename(url, version) {
  if (filenameRequest?.version === version) return filenameRequest.promise;
  const promise = (async () => {
    try {
      const info = await api.filename(url);
      if (version !== filenameVersion || !$('#new-dialog').open) return;
      filenameSuggestion = info;
      if (!filenameEdited || !$('#download-filename').value.trim()) {
        $('#download-filename').value = info.filename;
        filenameEdited = false;
      } else if (document.activeElement !== $('#download-filename')) {
        completeFilenameInput();
      }
      $('#filename-hint').textContent = info.extension
        ? `已识别 .${info.extension} · 只填写名称时自动补全后缀`
        : '未能识别文件格式，可手动填写后缀。';
    } catch {
      if (version === filenameVersion && $('#new-dialog').open) {
        $('#filename-hint').textContent = '暂未获取到文件名，可手动填写或直接开始下载。';
      }
    }
  })();
  filenameRequest = {version, promise};
  return promise;
}
function suggestName() {
  clearTimeout(filenameTimer);
  const version = ++filenameVersion;
  filenameSuggestion = null;
  filenameRequest = null;
  mediaPreview = null;
  mediaRequest = null;
  mediaSite = videoPlatform($('#download-url').value.trim());
  $('#media-panel').hidden = true;
  $('#media-source').hidden = !mediaSite;
  $('#platform-auth').hidden = !['视频号', '小红书'].includes(mediaSite);
  if (mediaSite === '视频号') {
    $('#platform-cookie-label').textContent = '元宝登录 Cookie（普通视频号分享链接需要）';
    $('#platform-cookie-hint').textContent = '若链接已包含 token 和 eid，可留空。仅用于本次向元宝请求播放页，不保存。';
  } else if (mediaSite === '小红书') {
    $('#platform-cookie-label').textContent = '小红书网页 Cookie（页面要求登录时填写）';
    $('#platform-cookie-hint').textContent = '仅用于本次向小红书请求笔记页，不保存。请使用刚复制的完整分享链接。';
  }
  $('#form-error').hidden = true;
  $('#parse-video').disabled = false;
  $('#parse-video').textContent = '解析视频';
  updateSubmitButton();
  const field = $('#download-filename');
  if (!filenameEdited) field.value = '';
  field.placeholder = '粘贴链接后自动识别文件名和后缀';
  $('#filename-hint').textContent = '后缀会根据下载文件自动补全。';
  if (mediaSite) {
    $('#media-source-label').textContent = `${mediaSite} 视频链接`;
    field.placeholder = '解析后自动填写视频名称';
    $('#filename-hint').textContent = '解析视频后选择清晰度和文件格式。';
    return;
  }
  const raw = $('#download-url').value.trim();
  try {
    const url = new URL(raw);
    if (!['http:', 'https:'].includes(url.protocol)) return;
    field.placeholder = decodeURIComponent(url.pathname.split('/').at(-1)) || '正在识别文件名…';
    $('#filename-hint').textContent = '正在识别文件名和格式…';
    filenameTimer = setTimeout(() => resolveFilename(raw, version), 450);
  } catch { /* The download form reports invalid URLs on submission. */ }
}

function videoPlatform(raw) {
  try {
    const url = new URL(videoUrl(raw));
    if (!['http:', 'https:'].includes(url.protocol)) return null;
    const sites = {'bilibili.com':'Bilibili', 'b23.tv':'Bilibili', 'bilibili.tv':'Bilibili', 'youtube.com':'YouTube', 'youtu.be':'YouTube', 'youtube-nocookie.com':'YouTube', 'douyin.com':'抖音', 'iesdouyin.com':'抖音', 'xiaohongshu.com':'小红书', 'xhslink.com':'小红书', 'xhslink.cn':'小红书', 'weixin.qq.com':'视频号', 'channels.weixin.qq.com':'视频号', 'tiktok.com':'TikTok', 'instagram.com':'Instagram', 'x.com':'X', 'twitter.com':'X', 'reddit.com':'Reddit', 'redd.it':'Reddit'};
    return Object.entries(sites).find(([host]) => url.hostname === host || url.hostname.endsWith(`.${host}`))?.[1] || null;
  } catch { return null; }
}

function videoUrl(raw) {
  const match = raw.match(/https?:\/\/[^\s<>"']+/i);
  return (match?.[0] || raw.trim()).replace(/[，。；;！!）)]+$/, '');
}

function updateSubmitButton() {
  const busy = mediaRequest?.version === filenameVersion;
  const unavailable = mediaPreview && !mediaPreview.formats.some(f => !f.needs_merge || mediaPreview.ffmpeg_available);
  $('#submit-download').disabled = submitting || busy || unavailable;
  $('#submit-download').innerHTML = busy ? '正在解析视频…' : mediaSite && !mediaPreview ? `${icon('search')}解析视频` : `${icon('download')}开始下载`;
}

function applyMediaFormat() {
  const format = mediaPreview?.formats.find(f => f.id === $('#media-format').value);
  if (!format) return;
  const previousExtension = filenameSuggestion?.extension;
  filenameSuggestion = {filename:format.filename, extension:format.extension};
  const field = $('#download-filename');
  if (!filenameEdited || !field.value.trim()) field.value = format.filename;
  else {
    if (previousExtension && field.value.toLowerCase().endsWith(`.${previousExtension}`)) {
      field.value = field.value.slice(0, -(previousExtension.length + 1)) + `.${format.extension}`;
    }
    completeFilenameInput();
  }
  $('#filename-hint').textContent = `已识别 .${format.extension} · 与所选下载格式一致`;
  const sourceInfo = mediaPreview.platform === 'Xiaohongshu'
    ? format.id === 'xhs-original' ? '原始文件直接保存，不转码'
      : mediaPreview.formats.some(f => f.id === 'xhs-original')
        ? '网页播放版可能含平台水印，建议选择原始文件'
        : '未取得可用原始文件，当前版本可能含平台水印'
    : null;
  $('#media-format-info').textContent = [format.total_bytes ? bytes(format.total_bytes) : '大小以下载时为准', format.needs_merge ? '下载完成后自动合并音视频' : format.audio_only ? '保存音频文件' : '包含完整音视频', sourceInfo].filter(Boolean).join(' · ');
}

async function parseVideo() {
  const version = filenameVersion;
  if (mediaRequest?.version === version) return mediaRequest.promise;
  const url = videoUrl($('#download-url').value.trim());
  mediaPreview = null;
  $('#media-panel').hidden = true;
  $('#form-error').hidden = true;
  $('#parse-video').disabled = true;
  $('#parse-video').textContent = '解析中…';
  const request = {version, promise:null};
  mediaRequest = request;
  const promise = (async () => {
    try {
      if (!connected) throw new Error('本地服务尚未连接，请稍后重试。');
      const cookie = ['视频号', '小红书'].includes(mediaSite) ? $('#platform-cookie').value.trim() || null : null;
      const result = await api.resolveMedia(url, cookie);
      if (version !== filenameVersion || !$('#new-dialog').open) return;
      mediaPreview = result;
      $('#media-title').textContent = result.title;
      const duration = result.duration_seconds ? `${Math.floor(result.duration_seconds / 60)}:${String(result.duration_seconds % 60).padStart(2, '0')}` : '';
      $('#media-meta').textContent = [result.platform, duration, `${result.formats.length} 个下载格式`].filter(Boolean).join(' · ');
      $('#media-format').innerHTML = result.formats.map(f => `<option value="${escape(f.id)}" ${f.needs_merge && !result.ffmpeg_available ? 'disabled' : ''}>${escape(f.label)}${f.needs_merge && !result.ffmpeg_available ? '（需要 FFmpeg）' : ''}</option>`).join('');
      const preferred = result.formats.find(f => !f.audio_only && (!f.needs_merge || result.ffmpeg_available)) || result.formats.find(f => !f.needs_merge || result.ffmpeg_available);
      if (preferred) $('#media-format').value = preferred.id;
      $('#media-panel').hidden = false;
      applyMediaFormat();
      if (!preferred) throw new Error('这些格式都需要合并音视频。请安装 FFmpeg 并重启服务后继续。');
    } catch (error) {
      if (version === filenameVersion && $('#new-dialog').open) {
        $('#form-error').textContent = error.name === 'TimeoutError' ? '解析超时，请检查网络后重试。' : error.message;
        $('#form-error').hidden = false;
      }
    } finally {
      if (version === filenameVersion) {
        mediaRequest = null;
        $('#parse-video').disabled = false;
        $('#parse-video').textContent = mediaPreview ? '重新解析' : '重新尝试';
        updateSubmitButton();
      }
    }
  })();
  request.promise = promise;
  updateSubmitButton();
  return promise;
}

$('#parse-video').addEventListener('click', parseVideo);
$('#media-format').addEventListener('change', applyMediaFormat);
document.querySelectorAll('[data-new]').forEach((button) => button.addEventListener('click', () => newDialog()));
document.querySelectorAll('.close-dialog').forEach((button) => button.addEventListener('click', () => button.closest('dialog').close()));
document.querySelectorAll('dialog').forEach((dialog) => dialog.addEventListener('click', (event) => {
  if (event.target === dialog) {
    const rect = dialog.getBoundingClientRect();
    if (event.clientX < rect.left || event.clientX > rect.right || event.clientY < rect.top || event.clientY > rect.bottom) dialog.close();
  }
}));
$('#download-url').addEventListener('input', suggestName);
$('#download-filename').addEventListener('input', () => { filenameEdited = true; });
$('#download-filename').addEventListener('blur', completeFilenameInput);
$('#new-dialog').addEventListener('close', () => { clearTimeout(filenameTimer); filenameVersion++; $('#platform-cookie').value = ''; });
$('#new-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  if (submitting) return;
  if (mediaSite && !mediaPreview) { await parseVideo(); return; }
  submitting = true;
  const button = $('#submit-download');
  button.disabled = true;
  $('#form-error').hidden = true;
  try {
    if (!connected) throw new Error('本地服务尚未连接，请确认 FFDownload 服务正在运行。');
    clearTimeout(filenameTimer);
    const url = $('#download-url').value.trim();
    const version = filenameVersion;
    if (!mediaSite && !filenameSuggestion) {
      button.textContent = '正在识别文件…';
      await resolveFilename(url, version);
    }
    if (!$('#new-dialog').open || version !== filenameVersion) return;
    completeFilenameInput();
    button.textContent = '正在添加…';
    const input = {
      url,
      filename: $('#download-filename').value.trim() || null,
      connections: Number($('#download-connections').value),
      sha256: $('#download-sha').value.trim() || null,
    };
    const task = mediaSite
      ? await api.createMedia({...input, preview_id:mediaPreview.id, format_id:$('#media-format').value})
      : await api.create(input);
    $('#new-dialog').close();
    setFilter('all');
    $('#search').value = '';
    toast(`已添加 · ${task.filename}`);
    await refresh();
  } catch (error) {
    $('#form-error').textContent = error.message;
    $('#form-error').hidden = false;
  } finally { submitting = false; updateSubmitButton(); }
});
function setFilter(next) {
  filter = next;
  document.querySelectorAll('[data-filter]').forEach((el) => {
    el.classList.toggle('selected', el.dataset.filter === filter);
    el.setAttribute('aria-current', el.dataset.filter === filter ? 'page' : 'false');
  });
  renderTasks();
}
$('#navigation').addEventListener('click', (event) => {
  const button = event.target.closest('[data-filter]');
  if (button) setFilter(button.dataset.filter);
});
$('#search').addEventListener('input', renderTasks);
$('#sort').addEventListener('change', renderTasks);

async function runAction(action, id) {
  if (action === 'detail') { selectedId = id; renderDetail(); $('#detail-dialog').showModal(); return; }
  if (action === 'reparse') {
    const task = snapshot.tasks.find(t => t.id === id);
    if (!task) return;
    if ($('#detail-dialog').open) $('#detail-dialog').close();
    newDialog(task.media?.source_url || task.url);
    await parseVideo();
    return;
  }
  if (pending.has(id)) return;
  pending.add(id); render();
  try {
    if (!connected) throw new Error('本地服务未连接，请稍后重试。');
    await api[action](id);
    if (action === 'pause') toast('正在保存分段进度…');
    if (action === 'resume') toast('任务已加入下载队列');
    if (action === 'remove') { $('#detail-dialog').close(); toast('已移除记录，磁盘上的文件和进度已保留'); }
    await refresh();
  } catch (error) { toast(error.message, true); }
  finally { pending.delete(id); render(); }
}
document.addEventListener('click', (event) => {
  const button = event.target.closest('[data-action]');
  if (button) runAction(button.dataset.action, button.dataset.id);
});
$('#pause-all').addEventListener('click', async () => {
  pending.add('all'); renderOverview();
  try { await api.pauseAll(); toast('正在保存所有进行中任务的进度…'); await refresh(); }
  catch (error) { toast(error.message, true); }
  finally { pending.delete('all'); renderOverview(); }
});
$('#sidebar-folder').addEventListener('click', async () => {
  try { await api.openFolder(); } catch (error) { toast(error.message, true); }
});

function renderDetail() {
  const task = snapshot.tasks.find((t) => t.id === selectedId);
  if (!task) { $('#detail-dialog').close(); return; }
  const pair = (name, value, mono = false) => `<div class="detail-pair"><dt>${name}</dt><dd class="${mono ? 'mono' : ''}">${escape(value)}</dd></div>`;
  $('#detail-content').innerHTML = `<h2 id="detail-title" class="detail-name">${escape(task.filename)}</h2><p class="detail-status">${stateLabels[task.state]}${task.demo ? ' · 本地体验文件' : ''}</p><dl class="detail-list">
    ${pair('文件大小', `${bytes(task.completed_bytes)} / ${bytes(task.total_bytes)}`)}
    ${pair('下载来源', displayUrl(task))}
    ${task.media ? pair('视频格式', `${task.media.platform} · ${task.media.label}`) : ''}
    ${pair('保存位置', `${snapshot.download_directory}/${task.filename}`)}
    ${pair('分段设置', `${task.connections} 路（当前活跃 ${task.active_connections} 路）`)}
    ${pair('添加时间', new Date(task.created_at).toLocaleString('zh-CN', {hour12:false}))}
    ${task.completed_at ? pair('完成时间', new Date(task.completed_at).toLocaleString('zh-CN', {hour12:false})) : ''}
    ${task.last_run_seconds != null ? pair('本次运行耗时', `${task.last_run_seconds.toFixed(2)} 秒（包含文件落盘与校验）`) : ''}
    ${task.average_mbps != null ? pair('全流程平均速率', `${task.average_mbps.toFixed(1)} Mbps · ${(task.average_mbps / 8).toFixed(2)} MB/s`) : ''}
    ${task.sha256 ? pair('SHA-256', task.sha256, true) : ''}</dl>
    ${task.error ? `<p class="form-error">${escape(task.error)}</p>` : ''}
    <p class="detail-note">${task.sha256 ? task.expected_sha256 ? '已与提供的 SHA-256 比对，内容校验通过。' : '已计算文件摘要。未提供预期摘要，尚未与发布方进行比对。' : '暂停后会保留分段进度，可在本地服务下次启动时继续。'}${task.demo ? '<br>这是通过 Rust 内核下载的本地数据，用于体验操作，不作为公网测速。' : ''}<br>移除记录会保留已经下载的文件及临时数据。</p>`;
  replacePreservingFocus($('#detail-actions'), `<button class="button danger" data-action="remove" data-id="${escape(task.id)}" data-focus-key="remove" ${active(task) || pending.has(task.id) ? 'disabled' : ''}>${icon('trash')}移除记录</button><div><button class="button secondary" id="copy-link" data-focus-key="copy-link">${icon('copy')}复制链接</button>${actionButton(task)}</div>`);
  $('#copy-link').onclick = async () => {
    try { await api.copyLink(task.id, task.url); toast('下载链接已复制'); }
    catch (error) { toast(isDesktop ? error.message : '浏览器未允许复制，请从原始来源复制链接', true); }
  };
}
document.addEventListener('keydown', (event) => {
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') {
    event.preventDefault();
    if ($('#detail-dialog').open) $('#detail-dialog').close();
    if (!$('#new-dialog').open) newDialog();
  }
});
document.addEventListener('dragover', (event) => {
  if ([...event.dataTransfer.types].includes('text/uri-list')) event.preventDefault();
});
document.addEventListener('drop', (event) => {
  const url = event.dataTransfer.getData('text/uri-list').split('\n').find((line) => /^https?:\/\//.test(line));
  if (url) { event.preventDefault(); if (!$('#new-dialog').open && !$('#detail-dialog').open) newDialog(url); }
});
if (isDesktop) {
  document.title = 'FFDownload';
  document.body.classList.add('desktop');
  api.info().then(info => {
    desktopPlatform = info.platform === 'windows' ? 'windows' : 'macos';
    const windows = desktopPlatform === 'windows';
    $('.tiny-badge').textContent = windows ? 'WIN' : 'MAC';
    $('.preview-badge').textContent = windows ? 'Windows 客户端' : 'macOS 客户端';
    $('#workspace-subtitle').textContent = windows ? '只属于这台电脑' : '只属于这台 Mac';
    $('#new-shortcut').textContent = windows ? 'Ctrl K' : '⌘ K';
    $('.sidebar-version span:last-child').textContent = `${info.version} · ${windows ? 'Windows' : 'macOS'}`;
    renderTasks();
  }).catch(error => toast(error.message, true));
  onDesktopEvent('desktop-new-download', () => {
    if ($('#detail-dialog').open) $('#detail-dialog').close();
    if (!$('#new-dialog').open) newDialog();
  }).catch(error => toast(error.message, true));
  onDesktopEvent('desktop-status', message => toast(message)).catch(error => toast(error.message, true));
}
poll();

initWorkbench({toast, onTaskCreated: refresh, hasActive: () => snapshot.tasks.some(active)});
