// "Show that section", asked from inside another one.
//
// The current section is the window's own state (App.svelte) and the sidebar
// is the only thing that used to change it. A job's menu has an item whose
// work lives in «Инструменты» — it leaves the request here and the window
// follows it.

import type { ViewName } from './types';

export const nav = $state({ request: null as ViewName | null });

export function goTo(view: ViewName): void {
  nav.request = view;
}
