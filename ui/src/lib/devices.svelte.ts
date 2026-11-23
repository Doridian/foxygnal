// SDR devices found by the engine's drivers (`GET /api/devices`), shared by
// the Devices panel and the device field in the inspector.

import { fetchDevices, type DeviceInfo, type Discovery } from './api';

class Devices {
  /** Per-driver results; `null` until the first search finishes. */
  drivers = $state<Discovery[] | null>(null);
  loading = $state(false);
  /** The request itself failed (engine unreachable). */
  error = $state<string | null>(null);

  /** Every device found, across drivers. */
  all: DeviceInfo[] = $derived((this.drivers ?? []).flatMap((d) => d.devices));

  /** Search again. Searching can take a few seconds (USB, network). */
  async refresh() {
    if (this.loading) return;
    this.loading = true;
    try {
      this.drivers = await fetchDevices();
      this.error = null;
    } catch (e) {
      this.error = String((e as Error).message ?? e);
    } finally {
      this.loading = false;
    }
  }

  /** Search once, the first time anyone asks. */
  ensure() {
    if (this.drivers === null && !this.loading) void this.refresh();
  }
}

export const devices = new Devices();
