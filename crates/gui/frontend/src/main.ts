import './styles/variables.css';
import './styles/global.css';
import App from './App.svelte';
import { mount } from 'svelte';
import { initTheme } from './lib/themeStore.svelte';
import { initI18n } from './lib/i18n.svelte';
import { ensurePlatform } from './lib/platformStore.svelte';

initTheme();
initI18n();
// Not awaited: the first draw goes by the webview's own guess.
void ensurePlatform();

const target = document.getElementById('app')!;

try {
  mount(App, { target });
} catch (e: unknown) {
  const msg = e instanceof Error ? `${e.message}\n\n${e.stack}` : String(e);
  target.innerHTML = `
    <div style="padding:32px;font-family:monospace;color:var(--error-text);background:var(--error-soft);border:1px solid var(--error-line);border-radius:8px;margin:32px;">
      <h2 style="margin:0 0 16px">App failed to start</h2>
      <pre style="white-space:pre-wrap;font-size:12px">${msg}</pre>
    </div>`;
}
