// SPDX-License-Identifier: AGPL-3.0-or-later
import { api } from "./api";
import type { RadioStation } from "../types/RadioStation";

// Both the sidebar and radio view reflect station changes immediately.
class Radio {
  stations = $state<RadioStation[]>([]);

  private revision = 0;

  async load(): Promise<void> {
    const revision = this.revision;
    const stations = await api.radio();
    if (revision === this.revision) this.stations = stations;
  }

  async save(station: {name: string; stream_url: string; homepage_url?: string | null}): Promise<RadioStation> {
    const existing = this.stations.find(s => s.stream_url === station.stream_url);
    if (existing) return existing;
    const saved = await api.createRadio(station);
    if (!saved) throw new Error("Couldn't save the station. Please retry.");
    this.revision++;
    this.stations = [...this.stations, saved].sort((a, b) => a.name.localeCompare(b.name));
    return saved;
  }

  async remove(id: string): Promise<void> {
    await api.deleteRadio(id);
    this.revision++;
    this.stations = this.stations.filter(s => s.id !== id);
  }
}

export const radio = new Radio();
