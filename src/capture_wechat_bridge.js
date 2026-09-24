// Minimal page-to-local-proxy bridge. Parsing, identity, keys and downloads live in Rust.
// A page can outlive several capture sessions. Obtain the current capability
// from the proxy-owned origin; never persist it or send it to a public service.
async function __ffdmCapturePost033(payload) {
  try {
    if (location.hostname !== 'channels.weixin.qq.com') return;
    const bridge = globalThis.__ffdmBridge033 || (globalThis.__ffdmBridge033 = {
      token: '', pending: null, refreshed: 0
    });
    const timeout = () => typeof AbortSignal !== 'undefined' && typeof AbortSignal.timeout === 'function'
      ? AbortSignal.timeout(5000) : undefined;
    const session = async () => {
      if (bridge.token && Date.now() - bridge.refreshed < 30000) return bridge.token;
      if (!bridge.pending) {
        bridge.pending = (async () => {
          const response = await fetch(__FFDM_BRIDGE__ + '/session', {
            mode: 'cors', credentials: 'omit', cache: 'no-store', referrerPolicy: 'origin', signal: timeout()
          });
          if (!response.ok) throw new Error('capture session unavailable');
          const data = await response.json();
          if (!/^[a-f0-9]{64}$/.test(data.token)) throw new Error('invalid capture session');
          bridge.token = data.token;
          bridge.refreshed = Date.now();
          return bridge.token;
        })().finally(() => { bridge.pending = null; });
      }
      return bridge.pending;
    };
    const post = token => fetch(__FFDM_BRIDGE__ + '/' + token + '/media', {
      method: 'POST', mode: 'cors', credentials: 'omit', cache: 'no-store', referrerPolicy: 'origin',
      headers: { 'Content-Type': 'text/plain' }, body: JSON.stringify(payload), signal: timeout()
    });
    const used = await session();
    const response = await post(used);
    if (response.status === 403) {
      // Another request may already have renewed this shared page capability.
      if (bridge.token === used) { bridge.token = ''; bridge.refreshed = 0; }
      await post(await session());
    }
  } catch (_) { /* Capture is optional; do not change playback or retry in a loop. */ }
}
function __ffdmCaptureStatus033(event) {
  try {
    if (location.hostname !== 'channels.weixin.qq.com') return;
    const cache = globalThis.__ffdmStatus033 || (globalThis.__ffdmStatus033 = new Map());
    const id = __FFDM_BRIDGE__ + event, now = Date.now();
    if (now - (cache.get(id) || 0) < 8000) return;
    if (cache.size >= 12) cache.delete(cache.keys().next().value);
    cache.set(id, now);
    __ffdmCapturePost033({source: 'status', event});
  } catch (_) {}
}
function __ffdmCaptureMedia033(desc, source) {
  try {
    __ffdmCaptureStatus033(source === 'detail' ? 'detail' : 'getter');
    if (location.hostname !== 'channels.weixin.qq.com' || !Array.isArray(desc?.media)) return;
    const media = desc.media.slice(0, 12).map(m => ({
      url: typeof m.url === 'string' ? m.url : '',
      urlToken: typeof m.urlToken === 'string' ? m.urlToken : '',
      decodeKey: typeof m.decodeKey === 'string' || typeof m.decodeKey === 'bigint'
        ? String(m.decodeKey) : Number.isSafeInteger(m.decodeKey) ? String(m.decodeKey) : '',
      mediaType: m.mediaType, fileSize: String(m.fileSize || '')
    }));
    const body = JSON.stringify({ source, description: String(desc.description || '').slice(0, 240), media });
    if (body.length > 32000) return;
    const cache = globalThis.__ffdmCaptureCache033 || (globalThis.__ffdmCaptureCache033 = new Map());
    const now = Date.now(), identity = source + ':' + media.map(m => m.url).join('|');
    if (now - (cache.get(identity) || 0) < 8000) return;
    if (cache.size >= 64) cache.delete(cache.keys().next().value);
    cache.set(identity, now);
    __ffdmCapturePost033(JSON.parse(body));
  } catch (_) { /* Capture must never prevent the original player from working. */ }
}
__ffdmCaptureStatus033('loaded');
