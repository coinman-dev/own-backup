// Are we the boot medium? One answer, asked once, read from anywhere.
//
// On the medium several parts of the interface describe a machine that keeps
// running and therefore have nothing to act on: schedules (the session ends at
// power-off), autostart and notifications (there is no logged-in desktop to
// return to), and the "install the service" warning (WinPE's registry lives in
// RAM, so there is nowhere to install it to). Hiding them is not cosmetic — an
// enabled schedule on the medium is a promise the medium cannot keep.
//
// The backend decides (see `media_mode` in the common crate); this only caches
// the answer, because it cannot change while the program runs.

import { api } from './api';

export const media = $state({
  /** True when running from the boot medium. */
  active: false,
  /** False until the first answer arrives, so nothing flashes on startup. */
  known: false,
});

/** Record what the service-health check already discovered. */
export function setMediaMode(active: boolean) {
  media.active = active;
  media.known = true;
}

/** For views that mount before the first health check has answered. */
export async function ensureMediaMode() {
  if (media.known) return;
  try {
    setMediaMode((await api.serviceHealth()).media);
  } catch {
    // Leave it unknown: showing the full interface is the safe default — the
    // medium merely looks like an ordinary install until the check succeeds.
  }
}
