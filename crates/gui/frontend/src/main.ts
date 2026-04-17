import './styles/variables.css';
import './styles/global.css';
import App from './App.svelte';
import { mount } from 'svelte';

const target = document.getElementById('app')!;

try {
  mount(App, { target });
} catch (e: unknown) {
  const msg = e instanceof Error ? `${e.message}\n\n${e.stack}` : String(e);
  target.innerHTML = `
    <div style="padding:32px;font-family:monospace;color:#c0392b;background:#fff5f5;border:1px solid #e74c3c;border-radius:8px;margin:32px;">
      <h2 style="margin:0 0 16px">App failed to start</h2>
      <pre style="white-space:pre-wrap;font-size:12px">${msg}</pre>
    </div>`;
}
