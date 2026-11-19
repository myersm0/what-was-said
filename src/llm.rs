use anyhow::Result;

/// Sampling settings for calls whose output should be reproducible, such as
/// claim extraction: unset fields keep the model's own defaults.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sampling {
	pub temperature: Option<f32>,
	pub seed: Option<i64>,
}

pub trait LlmBackend: Send + Sync {
	fn generate(
		&self,
		prompt: &str,
		model: &str,
		system: Option<&str>,
		format: Option<&str>,
	) -> Result<String>;

	fn chat(&self, prompt: &str, model: &str) -> Result<String> {
		self.generate(prompt, model, None, None)
	}

	/// `chat` with explicit sampling. Backends that can't set sampling fall
	/// back to `chat` with the model's defaults.
	fn chat_sampled(&self, prompt: &str, model: &str, sampling: Sampling) -> Result<String> {
		let _ = sampling;
		self.chat(prompt, model)
	}

	fn embed(&self, text: &str, model: &str) -> Result<Vec<f32>>;
}
