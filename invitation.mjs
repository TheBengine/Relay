export const BASE = 'https://thebengine.github.io/Relay/';
const QUERY = ['v', 'server', 'server_id', 'alt', 'iroh', 'irelay'];
const FRAGMENT = ['invite', 'access', 'iid', 'hd', 'hf', 'vk', 'it'];
const fail = () => { throw new Error('Invitationen er ikke gyldig. Bed din ven om et nyt link.'); };
const hex = value => /^[0-9a-f]{64}$/.test(value ?? '');

function fields(text, allowed) {
  const result = new Map();
  for (const [key, value] of new URLSearchParams(text)) {
    if (!allowed.includes(key) || result.has(key) || !value || /[\u0000-\u001f\u007f]/.test(value)) fail();
    result.set(key, value);
  }
  return result;
}

function address(value, relay = false) {
  let url;
  try { url = new URL(value); } catch { fail(); }
  if (!['http:', 'https:'].includes(url.protocol) || !url.hostname || url.username || url.password || url.hash || url.search) fail();
  if (relay && (url.protocol !== 'https:' || !['', '/'].includes(url.pathname) || value.length > 200)) fail();
  return url;
}

export function fromFragment(fragment) {
  if (typeof fragment !== 'string' || fragment.length > 4096 + BASE.length || /[\u0000-\u001f\u007f]/.test(fragment)) fail();
  const f = fields(fragment.replace(/^#/, ''), [...QUERY, ...FRAGMENT]);
  const get = name => f.get(name);
  if (!['1', '2', '3', '4'].includes(get('v')) || !get('server') || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(get('server_id') ?? '') || !/^[A-Za-z0-9_-]{1,128}$/.test(get('invite') ?? '')) fail();
  address(get('server'));
  if (get('v') !== '4' && ['alt', 'iroh', 'irelay', 'it'].some(name => f.has(name))) fail();
  if (get('v') === '1' && FRAGMENT.slice(1).some(name => f.has(name))) fail();
  if (get('v') === '2' && (!f.has('access') || ['iid', 'hd', 'hf', 'vk'].some(name => f.has(name)))) fail();
  const proof = ['iid', 'hd', 'hf', 'vk'].filter(name => f.has(name));
  if (get('v') === '3' && proof.length !== 4 || proof.length !== 0 && proof.length !== 4) fail();
  if (proof.length && (!hex(get('iid')) || !hex(get('hf')) || !/^[A-Za-z0-9_:.-]{1,128}$/.test(get('hd')) || !/^[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]$/.test(get('vk')))) fail();
  if (f.has('access')) {
    const u = address(get('access'));
    if (u.protocol !== 'https:' || !['login.tailscale.com', 'console.tailscale.com'].includes(u.hostname) || u.port || !/^\/admin\/invite\/[A-Za-z0-9_-]+$/.test(u.pathname) || get('access').length > 1024) fail();
  }
  if (f.has('alt')) {
    const list = get('alt').trim().split(/\s+/);
    if (list.length > 4) fail();
    for (const item of list) {
      const u = address(item);
      if (item.length > 200 || u.protocol === 'http:' && u.port !== '4580' || u.protocol === 'https:' && !['', '443', '4580'].includes(u.port)) fail();
    }
  }
  if (f.has('iroh') !== f.has('it') || f.has('irelay') && !f.has('iroh')) fail();
  if (f.has('iroh') && (!hex(get('iroh')) || !/^[A-Za-z0-9_-]{22,64}$/.test(get('it')))) fail();
  if (f.has('irelay')) address(get('irelay'), true);
  const query = new URLSearchParams();
  const secret = new URLSearchParams();
  for (const name of QUERY) if (f.has(name)) query.set(name, get(name));
  for (const name of FRAGMENT) if (f.has(name)) secret.set(name, get(name));
  const native = `relay://join?${query}#${secret}`;
  if (native.length > 4096) fail();
  return native;
}

export function fromNative(text) {
  if (typeof text !== 'string' || text.length > 4096 || /[\u0000-\u001f\u007f]/.test(text)) fail();
  let url;
  try { url = new URL(text.trim()); } catch { fail(); }
  if (url.protocol !== 'relay:' || url.hostname !== 'join' || url.pathname || url.port || url.username || url.password) fail();
  const query = fields(url.search.slice(1), QUERY);
  const secret = fields(url.hash.slice(1), FRAGMENT);
  return fromFragment(new URLSearchParams([...query, ...secret]).toString());
}

export function browserLink(native) {
  return BASE + '#' + fromNative(native).slice('relay://join?'.length).replace('#', '&');
}
