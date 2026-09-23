<script lang="ts">
  import { kbd, watchKeyboard } from '../lib/keyboardStore.svelte';

  // A password box with an eye.
  //
  // On the boot medium there is no taskbar and no language indicator, so a
  // passphrase typed in the wrong layout fails with no visible reason at all.
  // Being able to look at what was actually typed is the difference between
  // "the password is wrong" and "the keyboard is wrong" — and on a machine
  // booted from our own medium there is nobody around to shoulder-surf.
  interface Props {
    value: string;
    oninput: (value: string) => void;
    /** Enter in the field (submit). */
    onenter?: () => void;
    placeholder?: string;
    /** Draw the field as rejected (wrong password). */
    invalid?: boolean;
    /** 'grow' fills its row; 'inline' keeps a fixed, narrower width. */
    variant?: 'grow' | 'inline';
    /**
     * Start with the text visible. For a 48-digit BitLocker recovery key that
     * is the sane default: it is copied off a printout or a phone, never
     * memorised, and forty-eight masked digits hide the one typo that matters.
     * Windows' own recovery screen shows them too.
     */
    revealed?: boolean;
  }
  let {
    value,
    oninput,
    onenter,
    placeholder = 'Пароль',
    invalid = false,
    variant = 'grow',
    revealed = false,
  }: Props = $props();

  // Deliberately the INITIAL value, not a live one (audit AUD-13 asks this to
  // be judged by the contract rather than assumed a defect). `revealed` says
  // what the field starts as; after that the eye belongs to the user, and
  // following the prop would undo their click. The one caller keys its
  // `{#each}` by device, so every volume gets its own field and its own
  // starting state — nothing changes under a live instance.
  // svelte-ignore state_referenced_locally
  let shown = $state(revealed);

  // The layout and Caps Lock, right beside the eye. The eye answers "what did
  // I type"; these answer "why", before anything is typed — a field of dots
  // cannot show that the keyboard is in Russian or stuck in capitals.
  $effect(() => watchKeyboard());

  const langTitle = $derived(
    kbd.layout
      ? `Раскладка клавиатуры: ${kbd.layout.name}. Переключение — Alt+Shift или Win+Пробел.`
      : '',
  );
  const capsTitle = $derived(
    kbd.caps === true
      ? 'Caps Lock включён — буквы набираются заглавными'
      : kbd.caps === false
        ? 'Caps Lock выключен'
        : 'Caps Lock: станет известно после первого нажатия клавиши',
  );
</script>

<span class="pw-field {variant}">
  <input
    type={shown ? 'text' : 'password'}
    class:err={invalid}
    {placeholder}
    {value}
    autocomplete="off"
    spellcheck="false"
    oninput={(e) => oninput((e.currentTarget as HTMLInputElement).value)}
    onkeydown={(e) => {
      if (e.key === 'Enter' && onenter) {
        e.preventDefault();
        onenter();
      }
    }}
  />
  <span class="tail">
    <button
      type="button"
      class="eye"
      class:on={shown}
      title={shown ? 'Скрыть пароль' : 'Показать пароль'}
      aria-label={shown ? 'Скрыть пароль' : 'Показать пароль'}
      onclick={() => (shown = !shown)}
    >
      {#if shown}
        <!-- open eye: the password is visible -->
        <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">
          <path d="M1.8 12S5.4 5.4 12 5.4 22.2 12 22.2 12 18.6 18.6 12 18.6 1.8 12 1.8 12z" />
          <circle cx="12" cy="12" r="3.1" />
        </svg>
      {:else}
        <!-- struck-through eye: the password is hidden -->
        <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">
          <path d="M9.9 5.6A9.6 9.6 0 0 1 12 5.4c6.6 0 10.2 6.6 10.2 6.6a17 17 0 0 1-3.4 4.2M6.2 7.8A17 17 0 0 0 1.8 12S5.4 18.6 12 18.6c1.9 0 3.5-.5 4.9-1.3" />
          <path d="M10 10a2.8 2.8 0 0 0 4 4" />
          <path d="M3 3l18 18" />
        </svg>
      {/if}
    </button>
    {#if kbd.layout}
      <span class="lang" title={langTitle}>{kbd.layout.code}</span>
    {/if}
    <span class="caps" class:on={kbd.caps === true} role="img" title={capsTitle} aria-label={capsTitle}>
      <!-- the Caps Lock key: an outlined arrow over a bar -->
      <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
        <path d="M12 3.5 4.5 11.5H8.5V15.5H15.5V11.5H19.5Z" />
        <path d="M8.5 19.5h7" />
      </svg>
    </span>
  </span>
</span>

<style>
  .pw-field {
    position: relative;
    display: inline-flex;
    align-items: center;
  }
  .pw-field.grow {
    flex: 1 1 auto;
    min-width: 170px;
  }
  .pw-field.inline {
    flex: 0 0 auto;
  }

  input {
    width: 100%;
    /* Room for the eye, the layout and Caps Lock, so a long password never
       runs under them. */
    padding: 8px 88px 8px 12px;
    border: 1px solid var(--border);
    border-radius: 5px;
    font-size: 13px;
    font-family: var(--font);
    background: var(--card-bg);
    color: var(--text);
  }
  .pw-field.inline input {
    width: 230px;
    padding: 6px 84px 6px 9px;
    border-radius: 4px;
  }
  input.err {
    border-color: #d84a4a;
  }

  .tail {
    position: absolute;
    right: 4px;
    display: flex;
    align-items: center;
    gap: 2px;
  }

  .eye {
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 4px;
    background: none;
    border: none;
    border-radius: 4px;
    color: var(--text-muted);
    cursor: pointer;
  }
  .eye:hover {
    color: var(--head);
    background: rgba(127, 127, 127, 0.14);
  }
  .eye.on {
    color: var(--primary);
  }

  .lang {
    min-width: 24px;
    padding: 2px 4px;
    border: 1px solid var(--border);
    border-radius: 4px;
    font-size: 10.5px;
    font-weight: 700;
    letter-spacing: 0.3px;
    line-height: 1.3;
    text-align: center;
    color: var(--text-secondary);
    cursor: default;
    user-select: none;
  }

  /* Off: there, but quiet. On: the one thing in the field that asks to be
     noticed. */
  .caps {
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 2px;
    border-radius: 4px;
    color: var(--text-muted);
    opacity: 0.45;
    cursor: default;
  }
  .caps.on {
    color: #b7791f;
    background: rgba(251, 188, 5, 0.18);
    opacity: 1;
  }
</style>
