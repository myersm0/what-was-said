use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::{OpenAiAuth, OpenAiConfig};
use crate::llm::LlmBackend;

#[derive(Serialize)]
struct ChatMessage {
	role: String,
	content: String,
}

#[derive(Serialize)]
struct ChatRequest {
	model: String,
	messages: Vec<ChatMessage>,
	#[serde(skip_serializing_if = "Option::is_none")]
	response_format: Option<ResponseFormat>,
}

#[derive(Serialize)]
struct ResponseFormat {
	r#type: String,
}

#[derive(Deserialize)]
struct ChatResponse {
	choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
	message: ChatMessageResponse,
}

#[derive(Deserialize)]
struct ChatMessageResponse {
	content: String,
}

#[derive(Serialize)]
struct EmbeddingRequest {
	model: String,
	input: String,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
	data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
	embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct TokenResponse {
	access_token: String,
	expires_in: u64,
}

struct CachedToken {
	access_token: String,
	expires_at: Instant,
}

enum AuthStrategy {
	ApiKey(String),
	OAuth {
		token_url: String,
		client_id: String,
		client_secret: String,
		scope: String,
		cached_token: Mutex<Option<CachedToken>>,
	},
}

pub struct OpenAiClient {
	base_url: String,
	auth: AuthStrategy,
	client: reqwest::blocking::Client,
}

impl OpenAiClient {
	pub fn from_config(config: &OpenAiConfig) -> Result<Self> {
		let auth = match config.auth {
			OpenAiAuth::ApiKey => {
				let api_key = std::env::var("OPENAI_API_KEY")
					.context("OPENAI_API_KEY not set (required for auth = \"api_key\")")?;
				AuthStrategy::ApiKey(api_key)
			}
			OpenAiAuth::OAuth => {
				let token_url = config.oauth_token_url.as_ref()
					.context("oauth_token_url required when auth = \"oauth\"")?
					.clone();
				let scope = config.oauth_scope.as_ref()
					.context("oauth_scope required when auth = \"oauth\"")?
					.clone();
				let client_id = std::env::var("OAUTH2_CLIENT_ID")
					.context("OAUTH2_CLIENT_ID not set (required for auth = \"oauth\")")?;
				let client_secret = std::env::var("OAUTH2_CLIENT_SECRET")
					.context("OAUTH2_CLIENT_SECRET not set (required for auth = \"oauth\")")?;
				AuthStrategy::OAuth {
					token_url,
					client_id,
					client_secret,
					scope,
					cached_token: Mutex::new(None),
				}
			}
		};
		let client = reqwest::blocking::Client::builder()
			.timeout(Duration::from_secs(600))
			.build()
			.context("failed to build http client")?;
		Ok(OpenAiClient {
			base_url: config.base_url.trim_end_matches('/').to_string(),
			auth,
			client,
		})
	}

	pub fn from_env() -> Result<Self> {
		let config = OpenAiConfig {
			base_url: std::env::var("OPENAI_API_BASE")
				.unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
			auth: OpenAiAuth::ApiKey,
			oauth_token_url: None,
			oauth_scope: None,
		};
		Self::from_config(&config)
	}

	fn get_bearer_token(&self) -> Result<String> {
		match &self.auth {
			AuthStrategy::ApiKey(key) => Ok(key.clone()),
			AuthStrategy::OAuth { token_url, client_id, client_secret, scope, cached_token } => {
				{
					let guard = cached_token.lock().unwrap();
					if let Some(ref cached) = *guard {
						if Instant::now() < cached.expires_at {
							return Ok(cached.access_token.clone());
						}
					}
				}
				let params = [
					("client_id", client_id.as_str()),
					("client_secret", client_secret.as_str()),
					("scope", scope.as_str()),
					("grant_type", "client_credentials"),
				];
				let response: TokenResponse = self.client
					.post(token_url)
					.form(&params)
					.send()?
					.error_for_status()
					.context("OAuth token request failed")?
					.json()?;
				let margin = Duration::from_secs(60);
				let expires_at = Instant::now()
					+ Duration::from_secs(response.expires_in)
					- margin;
				let access_token = response.access_token.clone();
				*cached_token.lock().unwrap() = Some(CachedToken {
					access_token: response.access_token,
					expires_at,
				});
				Ok(access_token)
			}
		}
	}
}

impl LlmBackend for OpenAiClient {
	fn generate(
		&self,
		prompt: &str,
		model: &str,
		system: Option<&str>,
		format: Option<&str>,
	) -> Result<String> {
		let mut messages = Vec::new();
		if let Some(system_prompt) = system {
			messages.push(ChatMessage {
				role: "system".to_string(),
				content: system_prompt.to_string(),
			});
		}
		messages.push(ChatMessage {
			role: "user".to_string(),
			content: prompt.to_string(),
		});

		let response_format = match format {
			Some("json") => Some(ResponseFormat {
				r#type: "json_object".to_string(),
			}),
			_ => None,
		};

		let request = ChatRequest {
			model: model.to_string(),
			messages,
			response_format,
		};

		let token = self.get_bearer_token()?;
		let response: ChatResponse = self
			.client
			.post(format!("{}/chat/completions", self.base_url))
			.bearer_auth(&token)
			.json(&request)
			.send()?
			.error_for_status()
			.context("OpenAI API request failed")?
			.json()?;

		response
			.choices
			.into_iter()
			.next()
			.map(|c| c.message.content)
			.context("no response from OpenAI API")
	}

	fn embed(&self, text: &str, model: &str) -> Result<Vec<f32>> {
		let request = EmbeddingRequest {
			model: model.to_string(),
			input: text.to_string(),
		};

		let token = self.get_bearer_token()?;
		let response: EmbeddingResponse = self
			.client
			.post(format!("{}/embeddings", self.base_url))
			.bearer_auth(&token)
			.json(&request)
			.send()?
			.error_for_status()
			.context("OpenAI embeddings request failed")?
			.json()?;

		response
			.data
			.into_iter()
			.next()
			.map(|d| d.embedding)
			.context("no embedding in OpenAI response")
	}
}
