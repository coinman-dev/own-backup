<script lang="ts">
  import Modal from './Modal.svelte';
  import { api } from '../lib/api';
  import { m } from '../lib/paraglide/messages';
  import type { UpdateCheckResult } from '../lib/types';

  interface Props {
    info: UpdateCheckResult | null;
    onClose: () => void;
  }

  let { info, onClose }: Props = $props();
  let error: string | null = $state(null);

  async function openRelease() {
    if (!info?.release_url) return;
    error = null;
    try {
      await api.openUpdateRelease(info.release_url);
      onClose();
    } catch (e: unknown) {
      error = m.update_open_failed({ error: e instanceof Error ? e.message : String(e) });
    }
  }
</script>

<Modal
  open={info !== null}
  title={m.update_title()}
  onClose={onClose}
  onConfirm={openRelease}
  confirmLabel={m.update_open()}
  cancelLabel={m.update_later()}
>
  {#if info}
    <p class="update-message">
      {m.update_available({
        latest: info.latest_version ?? '',
        current: info.current_version,
      })}
    </p>
    <p class="update-help">{m.update_install_help()}</p>
    {#if error}<p class="update-error" role="alert">{error}</p>{/if}
  {/if}
</Modal>

<style>
  .update-message {
    margin: 0;
    color: var(--text);
    font-size: 14px;
    line-height: 1.55;
  }

  .update-help,
  .update-error {
    margin: 12px 0 0;
    font-size: 13px;
    line-height: 1.5;
  }

  .update-help {
    color: var(--text-secondary);
  }

  .update-error {
    color: var(--error-text);
  }
</style>
