// SPDX-License-Identifier: AGPL-3.0-or-later
// The track metadata editor: opens a review for a track, lets the user approve/reject each
// source's field observations (which the server folds into the canonical metadata).
import {
  api,
  type TrackMetadataReview,
  type MetadataFieldObservation,
  type MetadataApprovalState,
} from "./api";

class Metadata {
  trackId = $state<string | null>(null);
  review = $state<TrackMetadataReview | null>(null);
  loading = $state(false);

  async open(trackId: string): Promise<void> {
    this.trackId = trackId;
    this.review = null;
    this.loading = true;
    try {
      this.review = await api.metadataReview(trackId);
    } catch {
      // leave null
    } finally {
      this.loading = false;
    }
  }

  close(): void {
    this.trackId = null;
    this.review = null;
  }

  async setField(field: MetadataFieldObservation, state: MetadataApprovalState): Promise<void> {
    if (!this.trackId) return;
    await api.updateMetadataField(this.trackId, {
      source: field.source,
      observed_at_unix_seconds: field.observed_at_unix_seconds,
      field_name: field.field_name,
      value: field.value,
      approval_state: state,
    });
    try {
      this.review = await api.metadataReview(this.trackId);
    } catch {
      // keep prior
    }
  }
}

export const metadata = new Metadata();
