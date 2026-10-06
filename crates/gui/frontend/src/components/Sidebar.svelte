<script lang="ts">
  import type { ViewName, KeyboardLayout } from '../lib/types';
  import { api } from '../lib/api';
  import { media } from '../lib/mediaStore.svelte';
  import { appVersion, ensureAppVersion } from '../lib/appVersion.svelte';
  import { effectiveTheme, setTheme } from '../lib/themeStore.svelte';
  import { m } from '../lib/paraglide/messages';
  import logoUrl from '../assets/logo.png';

  interface Props {
    currentView: ViewName;
    onNavigate: (view: ViewName) => void;
  }

  let { currentView, onNavigate }: Props = $props();
  ensureAppVersion();

  // ---- keyboard layout, boot medium only -------------------------------
  //
  // WinPE has no taskbar, so nothing anywhere on screen says which layout the
  // keyboard is in — and the one place it matters most is a BitLocker/LUKS
  // passphrase, where the wrong layout is indistinguishable from the wrong
  // password. Installed Windows keeps its own indicator; we stay out of the
  // way there.
  //
  // Polled rather than pushed: the layout changes behind our back (Alt+Shift
  // is handled by Windows, not by us), and there is no event to subscribe to
  // from inside the WebView. Three Win32 calls per tick.
  let layout = $state<KeyboardLayout | null>(null);

  $effect(() => {
    if (!media.active) {
      layout = null;
      return;
    }
    let alive = true;
    const tick = async () => {
      try {
        const l = await api.keyboardLayout();
        if (alive) layout = l;
      } catch {
        // Older exe without the command: no indicator, nothing broken.
      }
    };
    tick();
    const id = setInterval(tick, 700);
    return () => {
      alive = false;
      clearInterval(id);
    };
  });

  interface NavItem {
    id: ViewName;
    /** A function, so the menu follows a change of language. */
    label: () => string;
    icon: string;
  }

  // Icons from the design (Own Backup.dc.html), its <rect>/<circle> spelled
  // as path data so every item stays one <path>.
  const rect = (x: number, y: number, w: number, h: number, r: number) =>
    `M${x + r} ${y}h${w - 2 * r}a${r} ${r} 0 0 1 ${r} ${r}v${h - 2 * r}a${r} ${r} 0 0 1-${r} ${r}` +
    `h-${w - 2 * r}a${r} ${r} 0 0 1-${r}-${r}v-${h - 2 * r}a${r} ${r} 0 0 1 ${r}-${r}z`;
  const circle = (cx: number, cy: number, r: number) =>
    `M${cx - r} ${cy}a${r} ${r} 0 1 0 ${2 * r} 0a${r} ${r} 0 1 0-${2 * r} 0z`;

  const navItems: NavItem[] = [
    {
      id: 'backup',
      label: m.nav_backup,
      icon: rect(8, 8, 12, 12, 2) + 'M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2',
    },
    {
      id: 'restore',
      label: m.nav_restore,
      icon: 'M4.5 10A8 8 0 0 1 19 8M19.5 3.5V8H15M19.5 14A8 8 0 0 1 5 16M4.5 20.5V16H9',
    },
    {
      id: 'boot',
      label: m.nav_boot,
      // Монитор на подставке, на экране — строки меню загрузки.
      icon: rect(3, 4, 18, 12, 2) + 'M8 20h8M12 16v4M7 8h6M7 11h4',
    },
    {
      id: 'tools',
      label: m.nav_tools,
      icon:
        rect(4, 4, 6.5, 6.5, 1.5) +
        rect(13.5, 4, 6.5, 6.5, 1.5) +
        rect(4, 13.5, 6.5, 6.5, 1.5) +
        rect(13.5, 13.5, 6.5, 6.5, 1.5),
    },
    {
      id: 'activity',
      label: m.nav_activity,
      icon: circle(12, 12, 8.5) + 'M12 7.5V12l3 2',
    },
    {
      id: 'settings',
      label: m.nav_settings,
      icon:
        'M10.3 3h3.4l.5 2.4 1.7.9 2.2-1 2.4 2.4-1 2.2.9 1.7 2.4.5v3.4l-2.4.5-.9 1.7 1 2.2-2.4 2.4-2.2-1-1.7.9-.5 2.4h-3.4l-.5-2.4-1.7-.9-2.2 1-2.4-2.4 1-2.2-.9-1.7L3 13.7v-3.4l2.4-.5.9-1.7-1-2.2 2.4-2.4 2.2 1 1.7-.9z' +
        circle(12, 12, 3),
    },
    {
      id: 'about',
      label: m.nav_help,
      icon: circle(12, 12, 8.5) + 'M12 11v5M12 8h.01',
    },
  ];
</script>

<nav class="sidebar">
  <div class="sidebar-header">
    <div class="logo">
      <svg width="28" height="28" viewBox="0 0 24 24" fill="none" stroke="var(--logo)" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
        <rect x="3" y="7" width="18" height="12" rx="2.5" />
        <path d="M7 7V4.5M12 7V4M17 7V4.5" />
        <circle cx="12" cy="13" r="3" />
      </svg>
    </div>
    <span class="app-title">OWN BACKUP</span>
  </div>

  <ul class="nav-list">
    {#each navItems as item}
      <li>
        <button
          class="nav-item"
          class:active={currentView === item.id}
          onclick={() => onNavigate(item.id)}
        >
          <svg
            class="nav-icon"
            width="20"
            height="20"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.8"
            stroke-linecap="round"
            stroke-linejoin="round"
          >
            <path d={item.icon} />
          </svg>
          <span class="nav-label">{item.label()}</span>
        </button>
      </li>
    {/each}
  </ul>

  <!-- The mark, large, in the part of the strip the menu leaves empty. It
       says nothing the header does not, so it is hidden from readers — and
       from the page when the window is too short: the menu must never start
       scrolling to make room for a picture. -->
  <div class="brand" aria-hidden="true">
    <img src={logoUrl} alt="" draggable="false" />
  </div>

  <div class="sidebar-footer">
    <div class="version-text">{appVersion.value ? `v${appVersion.value}` : ''}</div>
    {#if layout}
      <div
        class="kbd"
        title={m.nav_kbd_title({ name: layout.name })}
      >
        <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round">
          <rect x="2" y="6" width="20" height="12" rx="2" />
          <path d="M6 10h.01M10 10h.01M14 10h.01M18 10h.01M8 14h8" />
        </svg>
        <span>{layout.code}</span>
      </div>
    {/if}
    <!-- Quick switch; the three-way choice («Как в Windows» too) is in
         «Параметры → Оформление». -->
    <!-- A press on the half that is already lit changes nothing — and must
         not be remembered either. In «Как в Windows» one of the two is lit
         by the system, and saving that press as a choice quietly turned
         «Как в Windows» into «Тёмная» for good: the window looked the same
         and stopped following Windows. -->
    <div class="theme-switch" role="group" aria-label={m.nav_theme_group()}>
      <button
        title={m.nav_theme_light()}
        class:on={effectiveTheme() === 'light'}
        onclick={() => effectiveTheme() !== 'light' && setTheme('light')}
      >
        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round">
          <circle cx="12" cy="12" r="4" />
          <path d="M12 2.5v2M12 19.5v2M2.5 12h2M19.5 12h2M5.3 5.3l1.4 1.4M17.3 17.3l1.4 1.4M5.3 18.7l1.4-1.4M17.3 6.7l1.4-1.4" />
        </svg>
      </button>
      <button
        title={m.nav_theme_dark()}
        class:on={effectiveTheme() === 'dark'}
        onclick={() => effectiveTheme() !== 'dark' && setTheme('dark')}
      >
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linejoin="round">
          <path d="M20 14.5A8 8 0 0 1 9.5 4a8 8 0 1 0 10.5 10.5z" />
        </svg>
      </button>
    </div>
  </div>
</nav>

<style>
  .sidebar {
    width: var(--sidebar-width);
    min-width: var(--sidebar-width);
    height: 100%;
    background: var(--sidebar-bg);
    display: flex;
    flex-direction: column;
    user-select: none;
  }

  .sidebar-header {
    padding: 20px 16px 24px;
    display: flex;
    align-items: center;
    gap: 10px;
    border-bottom: 1px solid var(--sidebar-line);
  }

  .logo {
    flex-shrink: 0;
    display: flex;
    align-items: center;
    justify-content: center;
  }

  .app-title {
    font-size: 13px;
    font-weight: 700;
    letter-spacing: 1.5px;
    color: #ffffff;
    white-space: nowrap;
  }

  .nav-list {
    list-style: none;
    padding: 8px 0;
    flex: 1;
    overflow-y: auto;
  }

  .nav-item {
    display: flex;
    align-items: center;
    gap: 12px;
    width: 100%;
    padding: 11px 20px;
    background: none;
    border: none;
    color: var(--sidebar-text);
    font-size: 11.5px;
    font-weight: 700;
    letter-spacing: 0.15px;
    text-align: left;
    cursor: pointer;
    transition: background-color 0.12s ease, color 0.12s ease;
    border-left: 3px solid transparent;
  }

  .nav-item:hover {
    background: var(--sidebar-hover);
  }

  .nav-item.active {
    background: var(--sidebar-active);
    color: var(--sidebar-text-active);
    border-left-color: var(--sidebar-active);
  }

  .nav-icon {
    flex-shrink: 0;
    color: var(--sidebar-icon);
    transition: color 0.12s ease;
  }

  .nav-item.active .nav-icon {
    color: var(--sidebar-text-active);
  }

  .nav-label {
    white-space: nowrap;
  }

  .brand {
    flex-shrink: 0;
    display: flex;
    justify-content: center;
    padding: 8px 0 22px;
  }

  /* A white rim, 3px, that follows the mark's own outline. The picture is a
     cut-out on a transparent ground, so `border` would frame its box and
     leave the mark floating inside a rectangle. A drop shadow without blur is
     a shifted copy of the silhouette, and each one is cast by everything
     before it: one pixel along both axes and both diagonals, there and back,
     adds up to three on every side and stays nearly round at the corners.
     Then the mark and its rim together let a tenth of the strip through. */
  .brand img {
    width: 148px;
    height: auto;
    filter:
      drop-shadow(1px 0 0 #ffffff) drop-shadow(-1px 0 0 #ffffff)
      drop-shadow(0 1px 0 #ffffff) drop-shadow(0 -1px 0 #ffffff)
      drop-shadow(1px 1px 0 #ffffff) drop-shadow(-1px -1px 0 #ffffff)
      drop-shadow(1px -1px 0 #ffffff) drop-shadow(-1px 1px 0 #ffffff);
    opacity: 0.9;
  }

  /* Header, seven items and the footer take about 440px; the mark needs 160
     more. Below that it goes, rather than the menu. */
  @media (max-height: 640px) {
    .brand {
      display: none;
    }
  }

  .sidebar-footer {
    padding: 12px 18px 12px 20px;
    border-top: 1px solid var(--sidebar-line);
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .version-text {
    font-size: 11px;
    color: rgba(255, 255, 255, 0.6);
  }

  /* Layout indicator: same line as the version, pushed to the right. */
  .kbd {
    margin-left: auto;
    display: flex;
    align-items: center;
    gap: 5px;
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.4px;
    color: var(--sidebar-text);
    opacity: 0.85;
  }
  .kbd svg {
    flex-shrink: 0;
    opacity: 0.75;
  }

  /* Pushed right on its own; with the layout indicator present it follows it. */
  .theme-switch {
    margin-left: auto;
    display: flex;
    gap: 2px;
    padding: 2px;
    border-radius: 8px;
    background: rgba(255, 255, 255, 0.07);
    flex-shrink: 0;
  }
  .kbd + .theme-switch {
    margin-left: 0;
  }
  .theme-switch button {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 30px;
    height: 26px;
    border: none;
    border-radius: 6px;
    background: transparent;
    color: rgba(255, 255, 255, 0.74);
    transition: background-color 0.12s ease, color 0.12s ease;
  }
  .theme-switch button:hover {
    color: #ffffff;
  }
  .theme-switch button.on {
    background: rgba(255, 255, 255, 0.18);
    color: #ffffff;
  }
</style>
