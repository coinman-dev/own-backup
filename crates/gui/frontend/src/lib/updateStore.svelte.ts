import type { UpdateCheckResult } from './types';

export const updateNotification = $state<{ available: UpdateCheckResult | null }>({ available: null });

export function showAvailableUpdate(result: UpdateCheckResult): void {
  if (result.status === 'available') updateNotification.available = result;
}
