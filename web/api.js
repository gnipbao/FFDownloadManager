// The bundled desktop window invokes Rust directly; the browser keeps its HTTP adapter.
export const isDesktop = typeof window !== 'undefined' && !!window.__TAURI__?.core?.invoke;

async function invoke(command, args = {}) {
  try { return await window.__TAURI__.core.invoke(command, args); }
  catch (error) { throw new Error(typeof error === 'string' ? error : error?.message || '客户端操作失败'); }
}

export function onDesktopEvent(name, callback) {
  return isDesktop ? window.__TAURI__.event.listen(name, event => callback(event.payload)) : Promise.resolve(() => {});
}

async function request(path, method = 'GET', body, timeout = 15000) {
  const response = await fetch(`/api${path}`, {
    method,
    headers: { 'Content-Type': 'application/json', 'X-FFDM-Client': 'local-ui' },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(timeout),
  });
  if (!response.ok) {
    const data = await response.json().catch(() => null);
    throw new Error(data?.error || `请求失败（${response.status}）`);
  }
  return response.json();
}
const webApi = {
  snapshot: () => request('/tasks'),
  create: (task) => request('/tasks', 'POST', task),
  filename: (url) => request('/filename', 'POST', {url}),
  resolveMedia: (url) => request('/media/resolve', 'POST', {url}, 65000),
  createMedia: (task) => request('/media/tasks', 'POST', task, 30000),
  pause: (id) => request(`/tasks/${encodeURIComponent(id)}/pause`, 'POST'),
  resume: (id) => request(`/tasks/${encodeURIComponent(id)}/resume`, 'POST'),
  remove: (id) => request(`/tasks/${encodeURIComponent(id)}`, 'DELETE'),
  reveal: (id) => request(`/tasks/${encodeURIComponent(id)}/reveal`, 'POST'),
  pauseAll: () => request('/pause-all', 'POST'),
  openFolder: () => request('/folder/open', 'POST'),
  copyLink: (_id, url) => navigator.clipboard.writeText(url),
};

const desktopApi = {
  snapshot: () => invoke('desktop_snapshot'),
  create: task => invoke('desktop_create', {task}),
  filename: url => invoke('desktop_filename', {url}),
  resolveMedia: url => invoke('desktop_resolve_media', {url}),
  createMedia: task => invoke('desktop_create_media', {task}),
  pause: id => invoke('desktop_pause', {id}),
  resume: id => invoke('desktop_resume', {id}),
  remove: id => invoke('desktop_remove', {id}),
  reveal: id => invoke('desktop_reveal', {id}),
  pauseAll: () => invoke('desktop_pause_all'),
  openFolder: () => invoke('desktop_open_folder'),
  copyLink: id => invoke('desktop_copy_link', {id}),
  info: () => invoke('desktop_info'),
};

export const api = isDesktop ? desktopApi : webApi;
