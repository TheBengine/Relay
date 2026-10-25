import { fromFragment, fromNative, browserLink } from './invitation.mjs?v=20261025';
const element = id => document.getElementById(id);
let native = null;
let generation = 0;
let launchTimer;

function showInvitation() {
  const current = ++generation;
  clearTimeout(launchTimer);
  native = null;
  element('heading').textContent = 'Vi ses i Relay.';
  element('description').textContent = 'Åbn invitationslinket fra din ven for at komme videre til Relay.';
  element('invitation').hidden = true;
  element('share').hidden = true;
  element('empty-state').hidden = false;
  element('open-relay').removeAttribute('href');
  element('manual-copy').hidden = true;
  element('manual-copy').value = '';
  element('copy-status').textContent = '';
  element('share-status').textContent = '';
  element('launch-status').textContent = 'Vælg “Åbn Relay”, hvis browseren spørger.';
  if (!location.hash) return;
  try {
    if (location.search) throw new Error('Bed din ven om et nyt invitationslink.');
    native = fromFragment(location.hash);
    element('heading').textContent = 'Dine venner venter.';
    element('description').textContent = 'Åbn invitationen i Relay, og slut dig til din vens server.';
    element('invitation').hidden = false;
    element('empty-state').hidden = true;
    element('share').hidden = false;
    element('open-relay').href = native;
    // A browser may require a second click. Keep the real link visible and
    // never change its external-app permission or repeatedly trigger prompts.
    if (window.top === window.self) launchTimer = setTimeout(() => {
      if (current === generation && native) {
        try { location.assign(native); } catch { /* The visible link is the fallback. */ }
      }
    }, 250);
  } catch {
    element('heading').textContent = 'Vi mangler en gyldig invitation.';
    element('description').textContent = 'Linket er ufuldstændigt eller ugyldigt. Bed din ven om at kopiere en ny invitation fra Relay.';
  }
}

async function copy(text, status, fallback = false) {
  try {
    await navigator.clipboard.writeText(text);
    element(status).textContent = 'Kopieret.';
  } catch {
    element(status).textContent = 'Markér og kopiér invitationen nedenfor.';
    if (fallback) {
      element('open-help').open = true;
      element('manual-copy').value = text;
      element('manual-copy').hidden = false;
      element('manual-copy').focus();
      element('manual-copy').select();
    }
  }
}

element('open-relay').addEventListener('click', () => {
  clearTimeout(launchTimer);
  element('launch-status').textContent = 'Vælg “Åbn Relay” i browserens dialog. Sker der ingenting, kan du hente appen nedenfor.';
});
element('copy-native').addEventListener('click', () => native && copy(native, 'copy-status', true));
element('copy-web').addEventListener('click', () => native && copy(browserLink(native), 'share-status', true));
element('convert').addEventListener('click', () => {
  try {
    const converted = fromNative(element('old-invitation').value);
    element('old-invitation').value = '';
    element('convert-status').textContent = '';
    location.hash = browserLink(converted).split('#')[1];
  } catch {
    element('convert-status').textContent = 'Indsæt hele relay://-invitationen fra din ven.';
  }
});
window.addEventListener('hashchange', showInvitation);
showInvitation();
