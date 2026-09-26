import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { paraglideVitePlugin } from '@inlang/paraglide-js';

// Strip crossorigin attributes from HTML — Tauri's WebView serves assets
// from tauri://localhost which doesn't support CORS headers.
const removeCrossorigin = () => ({
  name: 'remove-crossorigin',
  transformIndexHtml(html: string) {
    return html.replace(/ crossorigin/g, '');
  },
});

// messages/{ru,en}.json → src/lib/paraglide (generated, git-ignored). The
// same options as the `i18n` script in package.json, which runs before
// svelte-check: the check needs the generated types before Vite starts.
// The locale is ours, not Paraglide's (lib/i18n.svelte.ts overrides
// getLocale), so its strategy is only a fallback that never reads storage.
const paraglide = paraglideVitePlugin({
  project: './project.inlang',
  outdir: './src/lib/paraglide',
  strategy: ['globalVariable', 'baseLocale'],
  emitTsDeclarations: true,
  outputStructure: 'locale-modules',
  emitReadme: false,
  emitPrettierIgnore: false,
});

export default defineConfig({
  plugins: [paraglide, svelte(), removeCrossorigin()],

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
