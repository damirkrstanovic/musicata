# musicata-ml — audio embeddings & tags (M7, experimental)

Status: **Phase 1 shipped & verified** (2026-06-27). `crates/musicata-ml`.

An optional, standalone service that analyzes a track's *sound*: it runs an ONNX audio model
and returns a **2048-d embedding** (for "sounds-like" similarity) plus **AudioSet tags** (genre /
instrument / mood-ish). It's the foundation for content-based recommendations — distinct from
the metadata- and history-based layers that already ship.

It follows the design in [recommendations.md](recommendations.md): a **separate process behind
an HTTP boundary**, so the core server never links the ML/ONNX stack. ML stays optional, runs
off the playback path, and can be disabled or run on another machine.

## The model

[PANNs **CNN14** (16 kHz)](https://huggingface.co/pranjal-pravesh/PANNs_CNN14_ONNX), exported to
ONNX. The decisive property: its input is a **raw waveform** (`input_audio: [batch, samples]`) —
the mel-spectrogram is *inside* the ONNX graph. So the entire Rust-side preprocessing is just
**decode → downmix to mono → resample to 16 kHz** (symphonia + rubato, the same crates the
server's Snapcast path uses); no fragile spectrogram code. Outputs:

- `embedding` `[1, 2048]` — the similarity vector.
- `clip_scores` `[1, 527]` — AudioSet class scores, mapped to the bundled 527 display names
  (`data/audioset_labels.txt`).

The model (~327 MB: `Cnn14_16k.onnx` + external `Cnn14_16k.onnx.data`) is **not** in the repo —
it's fetched + cached at runtime (Immich pattern). `ort`'s `download-binaries` feature pulls the
ONNX Runtime itself, so no system install is needed.

**Verified end-to-end**: analyzing a Darkwood Dub track returned a 2048-d embedding and the top
tags `Music`, `Drum kit`, `Musical instrument`, `Speech`, `Drum`, `Snare drum`, `Percussion`,
`Bass drum` — correct for a percussion/bass-driven dub track.

## Run it

The crate is **excluded from the default workspace build** (heavy native deps), so build it
explicitly. It needs network on first run (ONNX Runtime + the model download):

```sh
cargo build -p musicata-ml --release

MUSICATA_ML_MODEL=./Cnn14_16k.onnx \
MUSICATA_ML_MODEL_URL=https://huggingface.co/pranjal-pravesh/PANNs_CNN14_ONNX/resolve/main/Cnn14_16k.onnx \
MUSICATA_ML_DATA_URL=https://huggingface.co/pranjal-pravesh/PANNs_CNN14_ONNX/resolve/main/Cnn14_16k.onnx.data \
  musicata-ml            # listens on 127.0.0.1:3091 (set MUSICATA_ML_ADDR to change)
```

## Performance

**CPU only — no GPU.** A **release** build does **~1 s per track** (the whole `testdata` library,
123 tracks, in ~110 s); inference is sub-second and the cost is mostly the audio decode. GPU
acceleration was investigated (CUDA/ROCm/MIGraphX/WebGPU execution providers) and **dropped** — on
the test hardware it gave no speedup over release CPU for this model, so it wasn't worth the
build/deploy complexity. **Build the service in release** (`cargo build -p musicata-ml --release`);
a debug build is much slower because the audio *decode* is unoptimized.

## HTTP API

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/health` | `{ status, model }`. |
| GET | `/info` | `{ model, version, embedding_dim, sample_rate, tags }`. |
| POST | `/analyze` | Body = raw audio bytes → `{ embedding: [f32; 2048], dim, tags: [{label, score}] }`. |

## Phased plan

- **Phase 1 — the service. ✅ Done** (this). Decode + ONNX inference + HTTP; decode unit-tested,
  inference verified against real tracks.
- **Phase 2 — server integration. ✅ Done.** sqlite-vec is **standard storage** (loaded on every
  connection); the `track_embedding` `vec0` index + `track_features` table hold embeddings + tags.
  A **scheduled** `ml_loop` (off by default; user-set daily time, default **02:00 local**; manual
  `POST /api/ml/analyze` to run now) posts un-analyzed tracks to the service and stores the
  results. Settings (`ml_enabled` / `ml_service_url` / `ml_schedule`) are in `/api/settings`;
  status at `/api/ml/status`. The service analyzes a centered ~15 s excerpt per track. Verified
  end-to-end on the real library (correct embeddings + tags stored, KNN similarity works).
- **Phase 3 — consume it.** KNN "sounds-like" similarity feeding recommendations/radio; tags as
  browse facets / smart-playlist criteria.
- **Phase 4 — packaging.** A separate optional container image for `musicata-ml` (not in the
  slim server image).
