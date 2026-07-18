use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct EmbedClient {
    base_url: String,
    http: reqwest::Client,
}

#[derive(Debug, Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: Vec<&'a str>,
}

#[derive(Debug, Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedData>,
}

#[derive(Debug, Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
    index: usize,
}

impl EmbedClient {
    pub fn new(base_url: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()?;
        Ok(Self {
            base_url: normalize_base_url(base_url),
            http,
        })
    }

    #[allow(dead_code)]
    pub async fn embed_batch(&self, model: &str, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed_batch_with_prefix(model, texts, None).await
    }

    /// Embed texts, optionally applying a per-item prefix (e.g. Nomic task prefixes).
    pub async fn embed_batch_with_prefix(
        &self,
        model: &str,
        texts: &[&str],
        prefix: Option<&str>,
    ) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let owned: Vec<String> = match prefix {
            Some(p) => texts.iter().map(|t| format!("{p}{t}")).collect(),
            None => texts.iter().map(|t| (*t).to_string()).collect(),
        };
        let input: Vec<&str> = owned.iter().map(String::as_str).collect();
        let url = format!("{}/v1/embeddings", self.base_url);
        let body = EmbedRequest { model, input };
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("ollama embeddings {status}: {text}");
        }
        let parsed: EmbedResponse = resp.json().await.context("decode embeddings response")?;
        let mut ordered = vec![None; texts.len()];
        for item in parsed.data {
            if item.index >= ordered.len() {
                bail!("embedding index {} out of range", item.index);
            }
            ordered[item.index] = Some(item.embedding);
        }
        ordered
            .into_iter()
            .enumerate()
            .map(|(i, v)| v.ok_or_else(|| anyhow!("missing embedding for index {i}")))
            .collect()
    }

    pub async fn ping(&self) -> Result<()> {
        let url = format!("{}/api/tags", self.base_url);
        let resp = self.http.get(&url).send().await.context("GET /api/tags")?;
        if !resp.status().is_success() {
            bail!("ollama not healthy: {}", resp.status());
        }
        Ok(())
    }

    /// Map model name → digest from `GET /api/tags` (best-effort).
    pub async fn model_digests(&self) -> Result<std::collections::HashMap<String, String>> {
        #[derive(Deserialize)]
        struct Tags {
            models: Vec<TagModel>,
        }
        #[derive(Deserialize)]
        struct TagModel {
            name: String,
            digest: Option<String>,
        }

        let url = format!("{}/api/tags", self.base_url);
        let resp = self.http.get(&url).send().await.context("GET /api/tags")?;
        if !resp.status().is_success() {
            bail!(
                "ollama tags {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
        }
        let parsed: Tags = resp.json().await.context("decode /api/tags")?;
        let mut map = std::collections::HashMap::new();
        for m in parsed.models {
            if let Some(d) = m.digest {
                map.insert(m.name.clone(), d.clone());
                if let Some((base, _)) = m.name.split_once(':') {
                    map.entry(base.to_string()).or_insert(d);
                }
            }
        }
        Ok(map)
    }

    pub async fn model_digest(&self, model: &str) -> Result<String> {
        let digests = self.model_digests().await?;
        digests
            .get(model)
            .cloned()
            .or_else(|| digests.get(&format!("{model}:latest")).cloned())
            .with_context(|| format!("Ollama model digest not found for {model:?}"))
    }
}

/// Task prefixes for models that require them (Nomic Embed).
pub fn document_prefix(model: &str) -> Option<&'static str> {
    if model_is_nomic(model) {
        Some("search_document: ")
    } else {
        None
    }
}

pub fn query_prefix(model: &str) -> Option<&'static str> {
    if model_is_nomic(model) {
        Some("search_query: ")
    } else {
        None
    }
}

fn model_is_nomic(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("nomic-embed")
}

fn normalize_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

pub fn l2_normalize(v: &mut [f32]) {
    let mut sum = 0f32;
    for x in v.iter() {
        sum += x * x;
    }
    let norm = sum.sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

pub fn truncate_and_normalize(mut v: Vec<f32>, dims: usize) -> Result<Vec<f32>> {
    if v.is_empty() {
        bail!("empty embedding");
    }
    if dims == 0 {
        bail!("dims must be > 0");
    }
    if dims > v.len() {
        bail!(
            "requested dims {dims} exceeds native embedding length {}",
            v.len()
        );
    }
    v.truncate(dims);
    l2_normalize(&mut v);
    Ok(v)
}

pub fn f32s_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

pub fn bytes_to_f32s(bytes: &[u8]) -> Result<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        bail!("embedding blob length {} not multiple of 4", bytes.len());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        out.push(f32::from_le_bytes(chunk.try_into().unwrap()));
    }
    Ok(out)
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut sum = 0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        sum += x * y;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_unit_length() {
        let mut v = vec![3.0, 4.0];
        l2_normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-5);
        assert!((v[1] - 0.8).abs() < 1e-5);
    }

    #[test]
    fn truncate_renormalizes() {
        let v = vec![1.0, 0.0, 1.0, 0.0];
        let t = truncate_and_normalize(v, 2).unwrap();
        assert_eq!(t.len(), 2);
        assert!((t[0] - 1.0).abs() < 1e-5);
        assert!(t[1].abs() < 1e-5);
    }

    #[test]
    fn roundtrip_bytes() {
        let v = vec![0.1, -0.2, 0.3];
        let b = f32s_to_bytes(&v);
        let back = bytes_to_f32s(&b).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn cosine_identical_is_one() {
        let mut a = vec![1.0, 2.0, 3.0];
        l2_normalize(&mut a);
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn ranking_deterministic() {
        let q = {
            let mut v = vec![1.0, 0.0, 0.0];
            l2_normalize(&mut v);
            v
        };
        let mut cands = [
            (1i64, vec![0.0, 1.0, 0.0]),
            (2i64, vec![0.9, 0.1, 0.0]),
            (3i64, vec![0.5, 0.5, 0.0]),
        ];
        for (_, v) in cands.iter_mut() {
            l2_normalize(v);
        }
        cands.sort_by(|a, b| {
            cosine(&q, &b.1)
                .partial_cmp(&cosine(&q, &a.1))
                .unwrap()
                .then_with(|| a.0.cmp(&b.0))
        });
        assert_eq!(cands[0].0, 2);
    }
}
