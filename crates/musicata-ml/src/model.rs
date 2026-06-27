//! The ONNX model: a raw-waveform audio model (PANNs CNN14, 16 kHz) that outputs a 2048-d
//! `embedding` (for similarity) and 527 AudioSet `clip_scores` (mapped to human-readable tags).

use anyhow::{Context, Result, anyhow};
use ort::session::Session;
use ort::value::Tensor;
use serde::Serialize;

/// AudioSet's 527 class display names, in the model's output order. Bundled at build time.
const LABELS: &str = include_str!("../data/audioset_labels.txt");

pub struct AudioModel {
    session: Session,
    labels: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Tag {
    pub label: String,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Analysis {
    /// The embedding vector (for vector similarity).
    pub embedding: Vec<f32>,
    pub dim: usize,
    /// The highest-scoring AudioSet tags (genre / instrument / mood-ish).
    pub tags: Vec<Tag>,
}

impl AudioModel {
    pub fn load(model_path: &str) -> Result<Self> {
        let session = Session::builder()?
            .commit_from_file(model_path)
            .with_context(|| format!("load ONNX model {model_path}"))?;
        let labels: Vec<String> = LABELS
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect();
        Ok(Self { session, labels })
    }

    /// Run the model on a mono 16 kHz waveform → embedding + top tags.
    pub fn analyze(&mut self, samples: &[f32], top_tags: usize) -> Result<Analysis> {
        let input = Tensor::from_array(([1_usize, samples.len()], samples.to_vec()))?;
        let outputs = self.session.run(ort::inputs!["input_audio" => input])?;

        // Look outputs up by name rather than indexing: a model whose outputs are named
        // differently must yield a clean error, not a panic (which, run under the lock,
        // would poison it and brick the service).
        let embedding_out = outputs
            .get("embedding")
            .ok_or_else(|| anyhow!("model is missing the 'embedding' output"))?;
        let (_shape, embedding) = embedding_out
            .try_extract_tensor::<f32>()
            .context("extract embedding")?;
        let scores_out = outputs
            .get("clip_scores")
            .ok_or_else(|| anyhow!("model is missing the 'clip_scores' output"))?;
        let (_shape, scores) = scores_out
            .try_extract_tensor::<f32>()
            .context("extract clip_scores")?;

        let mut ranked: Vec<usize> = (0..scores.len()).collect();
        ranked.sort_by(|&a, &b| {
            scores[b]
                .partial_cmp(&scores[a])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let tags = ranked
            .into_iter()
            .take(top_tags)
            .map(|i| Tag {
                label: self
                    .labels
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| format!("class_{i}")),
                score: scores[i],
            })
            .collect();

        Ok(Analysis {
            dim: embedding.len(),
            embedding: embedding.to_vec(),
            tags,
        })
    }

    pub fn tag_count(&self) -> usize {
        self.labels.len()
    }
}

/// Download a model file (and its external `.data`, if present) to `dest` when missing.
pub fn ensure_model(dest: &str, url: &str, data_url: Option<&str>) -> Result<()> {
    if std::path::Path::new(dest).exists() {
        return Ok(());
    }
    tracing::info!("downloading model {url} → {dest}");
    download(url, dest)?;
    if let Some(data_url) = data_url {
        download(data_url, &format!("{dest}.data"))?;
    }
    Ok(())
}

fn download(url: &str, dest: &str) -> Result<()> {
    let response = ureq::get(url)
        .call()
        .map_err(|error| anyhow!("download {url}: {error}"))?;
    let mut reader = response.into_reader();
    let mut file = std::fs::File::create(dest).with_context(|| format!("create {dest}"))?;
    std::io::copy(&mut reader, &mut file).with_context(|| format!("write {dest}"))?;
    Ok(())
}
