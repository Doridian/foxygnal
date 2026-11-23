// Per-browser view preferences.

const KEY = 'foxygnal:prefs';

function load(): { overlay: boolean } {
  try {
    return { overlay: false, ...JSON.parse(localStorage.getItem(KEY) ?? '{}') };
  } catch {
    return { overlay: false };
  }
}

export const prefs = $state(load());

export function savePrefs() {
  try {
    localStorage.setItem(KEY, JSON.stringify(prefs));
  } catch {
    // Not remembered; fine.
  }
}
