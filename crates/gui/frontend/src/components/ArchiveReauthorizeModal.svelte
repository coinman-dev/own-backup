<script lang="ts">
  import ArchiveUnlockModal from './ArchiveUnlockModal.svelte';
  import Modal from './Modal.svelte';
  import { api } from '../lib/api';
  import { reloadJobs } from '../lib/jobStore.svelte';
  import type { ArchiveSecret, Job } from '../lib/types';
  import { m } from '../lib/paraglide/messages';

  let { job, onClose }: { job: Job; onClose: () => void } = $props();
  let busy = $state(false);
  let error = $state('');
  let complete = $state(false);

  async function authorize(secret: ArchiveSecret) {
    if (busy) return;
    busy = true;
    error = '';
    try {
      await api.reauthorizeArchive(job.id, secret);
      await reloadJobs();
      complete = true;
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }
</script>

{#if complete}
  <Modal open title={m.bsm_reauthorize_done_title()} confirmLabel={m.common_got_it()}
    cancelLabel="" {onClose} onConfirm={onClose}>
    <p>{m.bsm_reauthorize_done_message()}</p>
  </Modal>
{:else}
  <ArchiveUnlockModal open imageName={job.name} purpose="authorize" {busy} {error}
    onUnlock={authorize} onCancel={() => { if (!busy) onClose(); }} />
{/if}
