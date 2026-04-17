import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// Strip crossorigin attributes from HTML — Tauri's WebView serves assets
// from tauri://localhost which doesn't support CORS headers.
const removeCrossorigin = () => ({
  name: 'remove-crossorigin',
  transformIndexHtml(html: string) {
    return html.replace(/ crossorigin/g, '');
  },
});

export default defineConfig({
  plugins: [svelte(), removeCrossorigin()],

  // Prevent Vite from clearing the terminal so Tauri logs stay visible
  clearScreen: false,

  // Svelte publishes separate browser/server entry points. In this Tauri GUI
  // we must always prefer the browser runtime; otherwise `mount(...)` resolves
  // to the server stub and the app crashes on startup.
  resolve: {
    conditions: ['browser'],
  },

  // Tauri expects a fixed port; fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ['**/src-tauri/**'],
    },
  },

  // Relative paths so Tauri webview can resolve assets correctly.
  // Without base: './', Vite emits /assets/... (absolute) which breaks in Tauri.
  base: './',

  build: {
    target: 'es2021',
    outDir: 'dist',
    modulePreload: { polyfill: false },
  },
});
