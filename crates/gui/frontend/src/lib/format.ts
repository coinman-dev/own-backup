/**
 * Format bytes into a human-readable string such as "1.5 GiB" or "350 MiB".
 *
 * Binary prefixes (KiB/MiB/GiB) — the divisor is 1024, so "KB/MB/GB" would be
 * wrong: those mean powers of 1000. Disk and partition sizes here are all
 * computed in 1024-based units, and the labels must say so.
 */
export function formatBytes(bytes: number): string {
  if (bytes === 0) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
  const k = 1024;
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  const value = bytes / Math.pow(k, i);
  return `${value < 10 ? value.toFixed(2) : value < 100 ? value.toFixed(1) : Math.round(value)} ${units[i]}`;
}

/**
 * Format an ISO date string into "16 апреля 2026, 14:30" — local time, month
 * in the genitive the way a Russian date is read. It used to print English
 * month abbreviations ("16 Apr 2026") in an otherwise Russian interface.
 */
export function formatDate(iso: string): string {
  const d = new Date(iso);
  if (isNaN(d.getTime())) return iso;
  const months = [
    'января', 'февраля', 'марта', 'апреля', 'мая', 'июня',
    'июля', 'августа', 'сентября', 'октября', 'ноября', 'декабря',
  ];
  const day = d.getDate();
  const month = months[d.getMonth()];
  const year = d.getFullYear();
  const hours = d.getHours().toString().padStart(2, '0');
  const minutes = d.getMinutes().toString().padStart(2, '0');
  return `${day} ${month} ${year}, ${hours}:${minutes}`;
}

/**
 * Format a duration in seconds into "5 мин 32 с", "1 ч 12 мин" or "45 с"
 * (it was "5m 32s").
 */
export function formatDuration(seconds: number): string {
  if (seconds < 0) return '—';
  if (seconds < 60) return `${Math.round(seconds)} с`;
  const mins = Math.floor(seconds / 60);
  const secs = Math.round(seconds % 60);
  if (mins < 60) return `${mins} мин ${secs} с`;
  const hrs = Math.floor(mins / 60);
  const remainMins = mins % 60;
  return `${hrs} ч ${remainMins} мин`;
}


/** Format a millisecond duration as H:MM:SS or M:SS. */
export function formatClock(ms: number): string {
  if (!isFinite(ms) || ms < 0) return '—';
  const total = Math.round(ms / 1000);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const pad = (n: number) => String(n).padStart(2, '0');
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}
