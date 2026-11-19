use anyhow::Result;
use rusqlite::Connection;

use crate::llm::LlmBackend;
use crate::storage;

/// Stored vectors to drop before embedding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rebuild {
	/// Claim vectors only, embedded again with the recorded model.
	Claims,
	/// Chunk and claim vectors and the recorded model, so the configured
	/// model becomes the new one.
	All,
}

pub fn run(
	connection: &Connection,
	backend: &dyn LlmBackend,
	embed_model: &str,
	limit: Option<usize>,
	rebuild: Option<Rebuild>,
) -> Result<()> {
	match rebuild {
		Some(Rebuild::Claims) => {
			// Check first, so a mismatched model fails before anything is dropped.
			storage::check_embedding_model(connection, embed_model)?;
			storage::drop_claim_vectors(connection)?;
			println!("dropped claim vectors");
		}
		Some(Rebuild::All) => {
			let previous = storage::recorded_embedding_model(connection)?;
			storage::drop_all_vectors(connection)?;
			match previous {
				Some(previous) if previous != embed_model => {
					println!("dropped all vectors; switching from {} to {}", previous, embed_model);
				}
				_ => println!("dropped all vectors"),
			}
		}
		None => {}
	}
	storage::check_embedding_model(connection, embed_model)?;
	let chunk_pending = storage::count_chunks_without_embeddings(connection)?;
	let chunk_existing = storage::count_chunks_with_embeddings(connection)?;
	println!("chunk embeddings: {} existing, {} pending", chunk_existing, chunk_pending);

	if chunk_pending > 0 {
		let chunks = storage::get_chunks_without_embeddings(connection, limit)?;
		let total = chunks.len();
		println!("computing embeddings for {} chunks using {}...", total, embed_model);

		for (i, chunk) in chunks.iter().enumerate() {
			let embedding = backend.embed(&chunk.body, embed_model)?;
			if i == 0 {
				storage::adopt_embedding_model(connection, embed_model, embedding.len())?;
				storage::ensure_vec_table(connection, embedding.len())?;
			}
			storage::insert_embedding(connection, chunk.id, &embedding)?;
			if (i + 1) % 10 == 0 || i + 1 == total {
				eprint!("\r  {}/{}", i + 1, total);
			}
		}
		eprintln!();
	}

	let claim_pending = storage::count_claims_without_embeddings(connection)?;
	let claim_existing = storage::count_claims_with_embeddings(connection)?;
	println!("claim embeddings: {} existing, {} pending", claim_existing, claim_pending);

	if claim_pending > 0 {
		let claims = storage::get_claims_without_embeddings(connection, limit)?;
		let total = claims.len();
		println!("computing embeddings for {} claims using {}...", total, embed_model);

		for (i, claim) in claims.iter().enumerate() {
			let embedding = backend.embed(&claim.content, embed_model)?;
			if i == 0 {
				storage::adopt_embedding_model(connection, embed_model, embedding.len())?;
				storage::ensure_vec_claims_table(connection, embedding.len())?;
			}
			storage::insert_claim_embedding(connection, claim.id, &embedding)?;
			if (i + 1) % 10 == 0 || i + 1 == total {
				eprint!("\r  {}/{}", i + 1, total);
			}
		}
		eprintln!();
	}

	if chunk_pending == 0 && claim_pending == 0 {
		println!("all chunks and claims have embeddings");
	} else {
		println!("done");
	}

	Ok(())
}
