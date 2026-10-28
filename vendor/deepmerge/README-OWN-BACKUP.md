# deepmerge 4.3.1, patched for Own Backup

Used only to build the window's interface: `@sveltejs/vite-plugin-svelte`
merges its options with it (`crates/gui/frontend`, `overrides` in
`package.json`). Nothing of it is in the program.

Source: `npm pack deepmerge@4.3.1` (registry tarball, SHA-256
`4003a0a348276a95543fa969f561f6b5968629acbbb59d66018dd23e7b8f1d5c`), MIT
(`license.txt`).

Changes (2026-10-28), in `index.js`, `dist/cjs.js` and `dist/umd.js`:
`mergeObject()` skips the key `__proto__` both when copying the target and
when merging the source. Upstream checks it only when the target is an
object, so a source merged over a number or a string, or a target with an
own `__proto__` (JSON.parse makes one), set the prototype of the result —
GHSA-gcqx-w48m-w2h4 (deepmerge 4.2.0–4.3.1, no fixed release). From
`package.json` its `scripts` and `devDependencies` are removed (the package is
not built here), and `rollup.config.js` with them.

Drop this copy when a fixed deepmerge is released, or when the Svelte plugin
no longer depends on it.
