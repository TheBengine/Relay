import { fromFragment, fromNative, browserLink } from './invitation.mjs?v=20261025';
const element = id => document.getElementById(id);
let native = null;
let generation = 0;
let launchTimer;
let helpTimer;
let copyAttempt = 0;

function offerManualJoin(current) {
  clearTimeout(helpTimer);
  helpTimer = setTimeout(() => {
    if (current !== generation || !native) return;
    // Browsers do not tell us whether Relay is installed or opened. Offer an
    // alternative without reporting a failure or prompting again.
    element('launch-status').textContent = 'If Relay did not open: try “Open Relay” again, or copy the invitation below and paste it in the app.';
    element('open-help').open = true;
  }, 4000);
}

function showInvitation() {
  const current = ++generation;
  ++copyAttempt;
  clearTimeout(launchTimer);
  clearTimeout(helpTimer);
  native = null;
  element('heading').textContent = 'Your friends. Your server.';
  element('description').textContent = 'Chat, voice and screen sharing. Host your own community on a computer you choose.';
  element('invitation').hidden = true;
  element('share').hidden = true;
  element('empty-state').hidden = false;
  element('open-relay').removeAttribute('href');
  element('manual-copy').hidden = true;
  element('manual-copy').value = '';
  element('open-help').open = false;
  element('copy-status').textContent = '';
  element('share-status').textContent = '';
  element('launch-status').textContent = 'Choose “Open Relay” if your browser asks.';
  if (!location.hash) return;
  try {
    if (location.search) throw new Error('Ask your friend for a new invitation link.');
    native = fromFragment(location.hash);
    element('heading').textContent = 'Your friends are waiting.';
    element('description').textContent = 'Open this invitation in Relay to join your friend’s server.';
    element('invitation').hidden = false;
    element('empty-state').hidden = true;
    element('share').hidden = false;
    element('open-relay').href = native;
    // A browser may require a second click. Keep the real link visible and
    // never change its external-app permission or repeatedly trigger prompts.
    if (window.top === window.self) launchTimer = setTimeout(() => {
      if (current === generation && native) {
        try { location.assign(native); } catch { /* The visible link is the fallback. */ }
        offerManualJoin(current);
      }
    }, 250);
  } catch {
    element('heading').textContent = 'This invitation is incomplete.';
    element('description').textContent = 'The link is incomplete or invalid. Ask your friend to copy a new invitation from Relay.';
  }
}

async function copy(text, status, fallback = false) {
  const current = generation;
  const attempt = ++copyAttempt;
  element('copy-status').textContent = '';
  element('share-status').textContent = '';
  element('manual-copy').value = '';
  element('manual-copy').hidden = true;
  try {
    await navigator.clipboard.writeText(text);
    if (current !== generation || attempt !== copyAttempt) return;
    element(status).textContent = 'Copied.';
  } catch {
    if (current !== generation || attempt !== copyAttempt) return;
    element(status).textContent = 'Select and copy the invitation below.';
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
  if (!native) return;
  element('launch-status').textContent = 'Choose “Open Relay” in your browser’s dialog. You can also copy the invitation and paste it in the app.';
  offerManualJoin(generation);
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
    element('convert-status').textContent = 'Paste the complete relay:// invitation from your friend.';
  }
});
window.addEventListener('hashchange', showInvitation);
showInvitation();
