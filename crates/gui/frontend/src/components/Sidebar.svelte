<script lang="ts">
  import type { ViewName, KeyboardLayout } from '../lib/types';
  import { api } from '../lib/api';
  import { media } from '../lib/mediaStore.svelte';

  interface Props {
    currentView: ViewName;
    onNavigate: (view: ViewName) => void;
  }

  let { currentView, onNavigate }: Props = $props();

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
    label: string;
    icon: string;
  }

  const navItems: NavItem[] = [
    {
      id: 'backup',
      label: 'РЕЗЕРВНОЕ КОПИРОВАНИЕ',
      icon: 'M8 8h12v12H8zM16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2',
    },
    {
      id: 'restore',
      label: 'ВОССТАНОВЛЕНИЕ',
      icon: 'M4 4v5h5M20 20v-5h-5M4 9a8 8 0 0 1 14-4M20 15a8 8 0 0 1-14 4',
    },
    {
      id: 'boot',
      label: 'ЗАГРУЗКА КОМПЬЮТЕРА',
      // Материнская плата: корпус с ножками и экраном.
      icon: 'M4 5h16a1 1 0 0 1 1 1v9a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1zM7 19h10M9 16v3M15 16v3M7 9h5M7 12h3',
    },
    {
      id: 'tools',
      label: 'ИНСТРУМЕНТЫ',
      icon: 'M4 5a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v4a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5zm10 0a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v4a1 1 0 0 1-1 1h-4a1 1 0 0 1-1-1V5zM4 15a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v4a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1v-4zm10 0a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v4a1 1 0 0 1-1 1h-4a1 1 0 0 1-1-1v-4z',
    },
    {
      id: 'activity',
      label: 'АКТИВНОСТЬ',
      icon: 'M12 6v6l4 2m6-2a10 10 0 1 1-20 0 10 10 0 0 1 20 0z',
    },
    {
      id: 'settings',
      label: 'ПАРАМЕТРЫ',
      icon: 'M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8zm-1.5-5.3a1.5 1.5 0 0 1 3 0l.2 1.5a7 7 0 0 1 2 1.2l1.4-.6a1.5 1.5 0 0 1 2 .6l.6 1a1.5 1.5 0 0 1-.5 2l-1.2.9a7 7 0 0 1 0 2.4l1.2.9a1.5 1.5 0 0 1 .5 2l-.6 1a1.5 1.5 0 0 1-2 .6l-1.4-.6a7 7 0 0 1-2 1.2l-.2 1.5a1.5 1.5 0 0 1-3 0l-.2-1.5a7 7 0 0 1-2-1.2l-1.4.6a1.5 1.5 0 0 1-2-.6l-.6-1a1.5 1.5 0 0 1 .5-2l1.2-.9a7 7 0 0 1 0-2.4l-1.2-.9a1.5 1.5 0 0 1-.5-2l.6-1a1.5 1.5 0 0 1 2-.6l1.4.6a7 7 0 0 1 2-1.2l.2-1.5z',
    },
    {
      id: 'about',
      label: 'СПРАВКА',
      icon: 'M12 2a10 10 0 1 0 0 20 10 10 0 0 0 0-20zm0 5a1.25 1.25 0 1 1 0 2.5A1.25 1.25 0 0 1 12 7zm-1 4h2v6h-2v-6z',
    },
  ];
</script>

<nav class="sidebar">
  <div class="sidebar-header">
    <div class="logo">
      <svg width="28" height="28" viewBox="0 0 24 24" fill="none" stroke="#2d7dd2" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <rect x="2" y="6" width="20" height="12" rx="2" />
        <path d="M12 6V2M8 6V4M16 6V4" />
        <circle cx="12" cy="12" r="3" />
      </svg>
    </div>
    <span class="app-title">MY OWN BACKUP</span>
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
          <span class="nav-label">{item.label}</span>
        </button>
      </li>
    {/each}
  </ul>

  <div class="sidebar-footer">
    <div class="version-text">v0.1.0</div>
    {#if layout}
      <div
        class="kbd"
        title="Раскладка клавиатуры: {layout.name}. Переключение — Alt+Shift."
      >
        <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round">
          <rect x="2" y="6" width="20" height="12" rx="2" />
          <path d="M6 10h.01M10 10h.01M14 10h.01M18 10h.01M8 14h8" />
        </svg>
        <span>{layout.code}</span>
      </div>
    {/if}
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
    border-bottom: 1px solid rgba(255, 255, 255, 0.06);
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
    font-size: 12px;
    font-weight: 700;
    letter-spacing: 0.03em;
    text-align: left;
    cursor: pointer;
    transition: background-color 0.12s ease, color 0.12s ease;
    border-left: 3px solid transparent;
  }

  .nav-item:hover {
    background: var(--sidebar-hover);
    color: #ccdbe8;
  }

  .nav-item.active {
    background: var(--sidebar-active);
    color: var(--sidebar-text-active);
    border-left-color: var(--primary);
  }

  .nav-icon {
    flex-shrink: 0;
    color: var(--sidebar-icon);
    transition: color 0.12s ease;
  }

  .nav-item.active .nav-icon {
    color: var(--sidebar-text-active);
  }

  .nav-item:hover .nav-icon {
    color: #8eb4d8;
  }

  .nav-label {
    white-space: nowrap;
  }

  .sidebar-footer {
    padding: 12px 20px;
    border-top: 1px solid rgba(255, 255, 255, 0.06);
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .version-text {
    font-size: 11px;
    color: var(--sidebar-text);
    opacity: 0.6;
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
</style>
