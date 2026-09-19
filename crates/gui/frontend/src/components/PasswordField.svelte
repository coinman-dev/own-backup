<script lang="ts">
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
</span>

<style>
  .pw-field {
    position: relative;
    display: inline-flex;
    align-items: center;
  }
  .pw-field.grow {
    flex: 1 1 auto;
    min-width: 120px;
  }
  .pw-field.inline {
    flex: 0 0 auto;
  }

  input {
    width: 100%;
    /* Room for the eye, so a long password never runs under it. */
    padding: 8px 38px 8px 12px;
    border: 1px solid var(--border);
    border-radius: 5px;
    font-size: 13px;
    font-family: var(--font);
    background: var(--card-bg);
    color: var(--text);
  }
  .pw-field.inline input {
    width: 180px;
    padding: 6px 34px 6px 9px;
    border-radius: 4px;
  }
  input.err {
    border-color: #d84a4a;
  }

  .eye {
    position: absolute;
    right: 4px;
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
</style>
