import { api, isDesktop } from './api.js';
const $ = s => document.querySelector(s);
const escape = s => String(s ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'})[c]);
const size = n => n == null ? '大小未知' : n >= 1048576 ? `${(n/1048576).toFixed(1)} MiB` : `${(n/1024).toFixed(0)} KiB`;
export async function initWorkbench({toast, onTaskCreated, hasActive}) {
  let capabilities;
  try { capabilities = await api.capabilities(); }
  catch { toast('无法读取功能配置，请刷新页面重试', true); return; }
  const captureEnabled = capabilities.browser_capture || capabilities.application_capture;
  $('#tab-capture').hidden = !captureEnabled;
  $('#tab-speedtest').hidden = !capabilities.dev_mode;
  $('#workbench-tabs').hidden = !captureEnabled && !capabilities.dev_mode;
  if (!captureEnabled && !capabilities.dev_mode) {
    if (['#capture', '#speedtest'].includes(location.hash)) history.replaceState(null, '', location.pathname + location.search);
    return;
  }
  $('#capture-demo').hidden = !capabilities.dev_mode || isDesktop;
  $('#capture-modes').hidden = !capabilities.application_capture;
  $('#capture-application-tab').hidden = !capabilities.application_capture;
  let state = {running:false,resources:[]};
  let view = 'downloads', busy = false, downloading = new Set(), renderKey = '';
  let mode = capabilities.application_capture ? 'application' : 'browser', setup = null;
  function select(next) {
    if (next === 'capture' && !captureEnabled) next = 'downloads';
    if (next === 'speedtest' && !capabilities.dev_mode) next = 'downloads';
    view = next;
    document.querySelectorAll('[data-view]').forEach(el => { el.classList.toggle('selected', el.dataset.view === next); el.setAttribute('aria-selected', String(el.dataset.view === next)); });
    $('.overview').hidden = next !== 'downloads';
    $('.downloads-section').hidden = next !== 'downloads';
    $('#capture-view').hidden = next !== 'capture';
    $('#speedtest-view').hidden = next !== 'speedtest';
    history.replaceState(null, '', next === 'downloads' ? location.pathname : `#${next}`);
    update();
  }
  document.querySelectorAll('[data-view]').forEach(el => el.addEventListener('click', () => select(el.dataset.view)));
  $('#navigation').addEventListener('click', () => select('downloads'));
  document.querySelectorAll('[data-new]').forEach(el => el.addEventListener('click', () => select('downloads')));
  async function action(fn) { if (busy) return; busy = true; renderControls(); try { await fn(); await update(); } catch (e) { toast(e.message, true); if (mode === 'application') { $('#capture-setup-error').textContent = e.message; $('#capture-setup-error').hidden = false; } } finally { busy = false; renderControls(); } }
  function renderControls() {
    $('#capture-open').disabled = busy || state.browser_available === false;
    $('#capture-browser-manual').disabled = busy || state.browser_available === false;
    $('#capture-toggle').hidden = mode === 'browser' && !state.running;
    $('#capture-toggle').disabled = busy || (mode === 'application' && !state.running && (!setup?.trusted || !!setup?.error || !setup?.supported));
    $('#capture-toggle').textContent = state.running ? '停止捕获' : mode === 'application' ? '开启应用抓包' : '后台解析';
    $('#capture-app-start').disabled = busy || !setup?.trusted || !setup?.supported || !!setup?.error || !!state.system_proxy || state.recovery_pending;
    $('#capture-network').disabled = busy || !!state.system_proxy;
    ['capture-cert-open','capture-setup-refresh','capture-app-restore'].forEach(id => { $(`#${id}`).disabled = busy; });
    $('#capture-app-restore').hidden = !state.recovery_pending;
    $('#capture-system-status').textContent = state.proxy_issue ? '连接需处理' : state.system_proxy ? `已连接 ${state.system_proxy}` : '未启用';
    $('#capture-trust-instructions').hidden = !!setup?.trusted;
    $('#capture-app-start').textContent = state.system_proxy ? '网络已连接' : '开启系统代理抓包';
    $('#capture-system-status').classList.toggle('ready', !!state.system_proxy && !state.proxy_issue);
  }
  async function loadSetup(service) {
    setup = await api.captureSetup(service);
    const select = $('#capture-network');
    select.innerHTML = setup.services.length ? setup.services.map(name => `<option value="${escape(name)}">${escape(name)}</option>`).join('') : '<option value="">请手动设置应用代理</option>';
    select.value = setup.service || '';
    $('#capture-trust-status').textContent = setup.trusted ? '已信任' : '等待信任';
    $('#capture-trust-status').classList.toggle('ready', setup.trusted);
    $('#capture-cert-name').textContent = setup.certificate_name;
    $('#capture-fingerprint').textContent = setup.fingerprint.match(/.{1,2}/g).join(':');
    $('#capture-upstream').textContent = setup.upstream ? `保留原代理 ${setup.upstream} 作为上游，停止时恢复原设置。` : '抓包代理直接连接网络，停止时恢复原设置。';
    $('#capture-setup-error').textContent = setup.error || (setup.supported ? '' : '本轮自动配置仅支持 macOS。其他系统可下载证书，并手动配置应用代理。');
    $('#capture-setup-error').hidden = !$('#capture-setup-error').textContent;
    renderControls();
  }
  function selectMode(next) {
    mode = next === 'application' && capabilities.application_capture ? 'application' : 'browser';
    document.querySelectorAll('[data-capture-mode]').forEach(el => { el.classList.toggle('selected', el.dataset.captureMode === mode); el.setAttribute('aria-selected', String(el.dataset.captureMode === mode)); });
    $('#capture-browser-panel').hidden = mode !== 'browser';
    $('#capture-application-panel').hidden = mode !== 'application';
    $('#capture-application-help').hidden = mode !== 'application';
    $('#capture-heading-title').textContent = mode === 'application' ? '从应用发现媒体。' : '后台查找媒体。';
    $('#capture-heading-description').textContent = mode === 'application' ? '在微信或音乐客户端播放，发现的资源会出现在下方。' : '粘贴网页链接，自动加载并发现可下载的音视频。';
    renderKey = ''; render();
    if (mode === 'application' && !setup) action(() => loadSetup());
  }
  document.querySelectorAll('[data-capture-mode]').forEach(button => button.onclick = () => selectMode(button.dataset.captureMode));
  const startApplication = async () => { await api.captureApplicationStart($('#capture-network').value); toast('应用抓包已开启，请回到微信或音乐客户端重新播放'); await loadSetup(); };
  $('#capture-app-start').onclick = () => action(startApplication);
  $('#capture-app-restore').onclick = () => action(async () => { await api.captureRestore(); toast('已处理原代理恢复；其他软件的新设置会保留'); await loadSetup($('#capture-network').value); });
  $('#capture-cert-open').onclick = () => action(async () => { await api.captureCertificateOpen(); toast('请在钥匙串中导入并信任证书，完成后刷新状态'); });
  $('#capture-setup-refresh').onclick = () => action(() => loadSetup($('#capture-network').value));
  $('#capture-network').onchange = () => action(() => loadSetup($('#capture-network').value));
  async function open(raw, visible = false) {
    const match = raw.match(/https?:\/\/[^\s<>"'，。]+/i);
    if (!match) throw new Error('请输入完整的视频网页链接');
    await api.captureBrowser(match[0], visible);
    toast(visible ? '已打开处理窗口，请自行登录或播放' : '正在后台加载网页并检测媒体');
  }
  $('#capture-open-form').addEventListener('submit', e => { e.preventDefault(); action(() => open($('#capture-url').value)); });
  $('#capture-browser-manual').onclick = () => action(() => open($('#capture-url').value, true));
  $('#capture-toggle').onclick = () => action(async () => { if (state.running) { await api.captureStop(); if (setup) await loadSetup($('#capture-network').value); } else if (mode === 'application') await startApplication(); else await open($('#capture-url').value); });
  $('#capture-demo').onclick = () => action(() => open(`${location.origin}/capture-demo`));
  $('#capture-clear').onclick = () => action(() => api.captureClear());
  async function download(id) {
    if (downloading.has(id)) return;
    downloading.add(id); renderKey = ''; render();
    try { await api.captureDownload(id, Number($('#capture-connections').value)); toast('已加入下载任务'); await onTaskCreated(); }
    catch (e) { toast(e.message, true); }
    finally { downloading.delete(id); renderKey = ''; render(); }
  }
  $('#capture-resources').onclick = e => {
    const button = e.target.closest('[data-capture-download]'); if (!button) return;
    const item = state.resources.find(x => x.id === button.dataset.captureDownload); if (!item) return;
    if (item.encrypted && !item.has_key) { toast('请在微信重新打开此视频详情并播放，等待自动关联解密信息'); return; }
    download(item.id);
  };
  function render() {
    $('#capture-status').textContent = state.proxy_issue ? '代理连接需处理' : state.running ? state.system_proxy ? '应用抓包中' : mode === 'application' ? '应用代理未接入' : state.browser?.phase === 'finished' ? '本轮检测完成' : '正在捕获' : state.recovery_pending ? '代理待恢复' : '未启动';
    const phase = state.browser?.phase;
    const found = state.resources.some(r => r.captured_at >= (state.browser?.started_at || 0));
    $('#capture-browser-status').textContent = state.browser_available === false ? '需要先安装 Chrome 或 Edge 才能在后台加载网页。' : ({starting:'正在启动浏览器…', scanning:'正在后台加载并尝试播放媒体，通常在 30 秒内完成。', manual:'请在打开的窗口中自行登录、完成验证或播放；之后可再次点击“后台解析”。', finished:found ? '本轮检测已完成，请在下方选择资源下载。' : '本轮未发现媒体，可打开窗口检查登录状态或手动播放。', failed:'后台检测未完成，可重新尝试或打开窗口处理。', closed:'处理窗口已关闭，可重新后台解析。', stopped:'后台检测已停止。'})[phase] || '等待网页链接';
    $('#capture-status').classList.toggle('running', state.running);
    renderControls();
    $('#capture-proxy').textContent = state.proxy || '启动后显示';
    $('#capture-observed').textContent = `${state.observed || 0} 个请求 · 已合并 ${state.merged || 0} 次重复请求${state.errors ? ` · ${state.errors} 次转发失败` : ''}`;
    const wechat = state.resources.filter(r => r.platform === '视频号');
    const unconfirmed = wechat.filter(r => !r.detail_at).length;
    $('#capture-wechat-status').hidden = mode !== 'application' && !state.wechat_requests;
    const diag = state.wechat_diagnostics || {};
    const waitingDetails = mode === 'application' && !!state.system_proxy && state.wechat_requests > 0 && !state.bridge_reports;
    $('#capture-wechat-status').textContent = state.proxy_issue || (mode === 'application' && !state.system_proxy
      ? '尚未开启应用抓包。证书已信任后，点击“开启系统代理抓包”即可，无需手动修改 Wi-Fi。'
      : diag.last_issue || (waitingDetails
        ? diag.loaded ? '代理与页面回传已接通，正在等待视频详情。无需重新配置 Wi-Fi。'
          : state.adapted_scripts ? '网络已连接，但视频号详情回传尚未接通；这是播放器适配问题，无需反复调整 Wi-Fi。'
          : '已收到微信流量，尚未加载视频号播放详情。请打开具体视频的详情页。'
        : state.bridge_reports ? `已关联 ${state.bridge_reports} 次视频信息。${unconfirmed ? `另有 ${unconfirmed} 个网络候选，可能是预加载。` : ''}`
        : state.system_proxy ? '网络已连接，请在微信打开具体视频并播放。' : '等待播放。'));
    $('#capture-wechat-diagnostics').hidden = mode !== 'application' && !state.wechat_requests;
    $('#capture-wechat-counters').textContent = `相关请求 ${state.wechat_requests || 0} · 播放页 ${state.wechat_pages || 0} · 适配脚本 ${state.adapted_scripts || 0} · 回传连接 ${diag.connections || 0} · 回传请求 ${diag.requests || 0} · 会话更新 ${diag.sessions || 0}/${diag.session_requests || 0} · 账号连接透传 ${diag.control_tunnels || 0} · 脚本运行 ${diag.loaded || 0} · 媒体读取 ${diag.getter || 0} · 详情读取 ${diag.detail || 0} · 有效媒体 ${state.bridge_reports || 0} · 被拒 ${diag.rejected || 0} · 无效媒体 ${diag.invalid_media || 0}`;
    $('#capture-wechat-assets').textContent = (diag.assets || []).join('\n');
    const query = $('#capture-search').value.trim().toLowerCase(), kind = $('#capture-kind').value, sort = $('#capture-sort').value, platform = $('#capture-platform').value;
    const scope = $('#capture-wechat-scope').value;
    const resources = state.resources.filter(r => (!query || `${r.platform} ${r.host} ${r.filename}`.toLowerCase().includes(query)) && (kind === 'all' || r.kind === kind) && (platform === 'all' || r.platform === platform) && (scope === 'all' || r.platform !== '视频号' || r.detail_at));
    resources.sort((a,b) => (b.detail_at || b.captured_at) - (a.detail_at || a.captured_at));
    if (sort === 'size') resources.sort((a,b) => (b.bytes || 0) - (a.bytes || 0));
    $('#capture-count').textContent = resources.length === state.resources.length ? resources.length : `${resources.length} / ${state.resources.length}`;
    const key = JSON.stringify(resources) + [...downloading].join() + mode + waitingDetails + unconfirmed + scope;
    if (key === renderKey) return; renderKey = key;
    $('#capture-resources').innerHTML = resources.length ? resources.map(r => `<article class="capture-resource"><div class="capture-type ${r.kind === 'audio' ? 'audio' : ''}">${escape(r.filename.split('.').at(-1).toUpperCase())}</div><div class="capture-resource-info"><strong title="${escape(r.filename)}">${escape(r.filename)}</strong><span><b class="resource-platform">${escape(r.platform)}</b>${escape(r.host)} · ${size(r.bytes)} · ${r.supports_range ? '支持分段' : '分段能力待探测'}</span><small>${r.detail_at ? '已关联详情页 · ' : r.platform === '视频号' ? '网络候选，可能为预加载 · ' : ''}${escape(r.note)}</small><small>${new Date(r.detail_at || r.captured_at).toLocaleTimeString()}${r.request_count > 1 ? ` · ${r.request_count} 次媒体请求已合并` : ''}</small></div><button class="button ${r.downloadable ? 'primary' : 'secondary'}" data-capture-download="${escape(r.id)}" ${!r.downloadable || r.encrypted && !r.has_key || downloading.has(r.id) ? 'disabled' : ''}>${downloading.has(r.id) ? '正在添加…' : r.downloadable ? r.encrypted && !r.has_key ? '等待播放详情' : '下载' : '暂不支持'}</button></article>`).join('') : `<div class="capture-empty">${unconfirmed && scope !== 'all' ? '已发现网络候选，尚未关联视频号详情' : state.resources.length ? '没有符合筛选条件的资源' : '还没有发现媒体资源'}<span>${unconfirmed && scope !== 'all' ? '请重新打开目标视频详情；也可以切换“视频号：全部网络候选”查看，候选不代表当前播放内容。' : waitingDetails ? '已收到微信流量，视频详情尚未关联；上方显示具体连接状态。' : mode === 'application' ? state.system_proxy ? '网络已连接，请在微信或音乐客户端播放内容。' : '点击“开启系统代理抓包”后，在微信或音乐客户端播放内容。' : '粘贴网页链接后点“后台解析”，资源会自动出现在这里。'}</span></div>`;
  }
  ['capture-search','capture-kind','capture-sort','capture-platform','capture-wechat-scope'].forEach(id => $(`#${id}`).addEventListener('input', () => { renderKey = ''; render(); }));
  let benchBusy = false;
  $('#benchmark-start').onclick = async () => {
    if (!capabilities.dev_mode || benchBusy) return;
    if (hasActive()) { toast('请先暂停正在进行的下载，以免影响测速结果', true); return; }
    benchBusy = true; $('#benchmark-start').disabled = true;
    try { await api.benchmarkStart(); await updateBenchmark(); } catch (e) { toast(e.message, true); }
    finally { benchBusy = false; }
  };
  async function updateBenchmark() {
    if (!capabilities.dev_mode) return;
    const result = await api.benchmark();
    $('#benchmark-start').disabled = result.running || benchBusy;
    $('#benchmark-start').textContent = result.running ? '正在测速…' : '开始受控测速';
    if (result.running) $('#benchmark-results').innerHTML = '<div class="capture-empty">正在依次下载与校验<span>测试真实数据传输、文件落盘与 SHA-256；请稍候。</span></div>';
    else if (result.error) $('#benchmark-results').textContent = result.error;
    else if (result.report) {
      const rows = result.report.samples.map(s => `<tr><td>${s.scenario === 'loopback_uncapped' ? '本机不限速' : '每连接 100 Mbps'}</td><td>${s.implementation === 'rust_stream' ? 'Rust 单流基线' : `Rust 下载器 · ${s.report.connections} 路`}</td><td>${s.report.total_seconds.toFixed(3)} s</td><td>${s.report.average_mbps.toFixed(1)} Mbps</td><td>${s.server_range_requests}</td><td>${s.report.sha256 ? '通过' : '未通过'}</td></tr>`).join('');
      $('#benchmark-results').innerHTML = `<div class="benchmark-table-wrap"><table class="benchmark-table"><thead><tr><th>场景</th><th>客户端</th><th>总耗时</th><th>平均速率</th><th>Range 请求</th><th>SHA-256</th></tr></thead><tbody>${rows}</tbody></table></div><p class="capture-footnote">${new Date(result.report.unix_time * 1000).toLocaleString()} · ${escape(result.report.platform)} · 单轮结果，计时包含落盘与校验。</p>`;
    }
  }
  let updating = false;
  async function update() {
    if (updating) return; updating = true;
    try { if (view === 'capture') { state = await api.capture(); render(); } else if (view === 'speedtest') await updateBenchmark(); }
    catch (e) { if (view === 'capture') $('#capture-status').textContent = '服务未连接'; }
    finally { updating = false; }
  }
  async function poll() { await update(); setTimeout(poll, 1500); }
  if (captureEnabled) selectMode(mode);
  select(['#capture','#speedtest'].includes(location.hash) ? location.hash.slice(1) : 'downloads'); poll();
}
