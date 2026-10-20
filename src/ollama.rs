use std::time::Duration;

use anyhow::Result;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::llm::{LlmBackend, Sampling};

#[derive(Serialize)]
struct GenerateRequest {
	model: String,
	prompt: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	system: Option<String>,
	stream: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	format: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	options: Option<GenerateOptions>,
}

#[derive(Serialize)]
struct GenerateOptions {
	#[serde(skip_serializing_if = "Option::is_none")]
	temperature: Option<f32>,
	#[serde(skip_serializing_if = "Option::is_none")]
	seed: Option<i64>,
}

impl GenerateOptions {
	fn from_sampling(sampling: Sampling) -> Option<Self> {
		(sampling != Sampling::default()).then_some(GenerateOptions {
			temperature: sampling.temperature,
			seed: sampling.seed,
		})
	}
}

#[derive(Deserialize)]
struct GenerateResponse {
	response: String,
}

#[derive(Serialize)]
struct EmbeddingRequest {
	model: String,
	prompt: String,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
	embedding: Vec<f32>,
}

/// Embedding one text, even after a cold model load, takes seconds; anything
/// longer means Ollama is stuck, and an interactive caller should hear so.
const EMBED_TIMEOUT: Duration = Duration::from_secs(120);

pub struct OllamaClient {
	base_url: String,
	client: reqwest::blocking::Client,
}

impl OllamaClient {
	pub fn new(base_url: &str) -> Self {
		OllamaClient {
			base_url: base_url.to_string(),
			client: reqwest::blocking::Client::builder()
				.timeout(Duration::from_secs(600))
				.build()
				.expect("failed to build http client"),
		}
	}

	fn generate_sampled(
		&self,
		prompt: &str,
		model: &str,
		system: Option<&str>,
		format: Option<&str>,
		sampling: Sampling,
	) -> Result<String> {
		let request = GenerateRequest {
			model: model.to_string(),
			prompt: prompt.to_string(),
			system: system.map(|s| s.to_string()),
			stream: false,
			format: format.map(|f| f.to_string()),
			options: GenerateOptions::from_sampling(sampling),
		};

		let response: GenerateResponse = parse_response(
			self.client
				.post(format!("{}/api/generate", self.base_url))
				.json(&request)
				.send()?,
		)?;

		Ok(response.response)
	}
}

impl LlmBackend for OllamaClient {
	fn generate(
		&self,
		prompt: &str,
		model: &str,
		system: Option<&str>,
		format: Option<&str>,
	) -> Result<String> {
		self.generate_sampled(prompt, model, system, format, Sampling::default())
	}

	fn chat_sampled(&self, prompt: &str, model: &str, sampling: Sampling) -> Result<String> {
		self.generate_sampled(prompt, model, None, None, sampling)
	}

	fn embed(&self, text: &str, model: &str) -> Result<Vec<f32>> {
		let request = EmbeddingRequest {
			model: model.to_string(),
			prompt: text.to_string(),
		};

		let response: EmbeddingResponse = parse_response(
			self.client
				.post(format!("{}/api/embeddings", self.base_url))
				.timeout(EMBED_TIMEOUT)
				.json(&request)
				.send()?,
		)?;

		Ok(response.embedding)
	}
}

/// Ollama reports failures such as an unknown model as a JSON body on a non-2xx
/// status; surface that text instead of a missing-field decode error.
fn parse_response<T: DeserializeOwned>(response: reqwest::blocking::Response) -> Result<T> {
	let status = response.status();
	if !status.is_success() {
		let body = response.text().unwrap_or_default();
		anyhow::bail!("ollama returned {}: {}", status, body.trim());
	}
	Ok(response.json()?)
}
