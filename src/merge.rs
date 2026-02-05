use crate::minhash::{is_short_entry, jaccard, minhash, minhash_with_context};
use crate::types::*;

const similarity_threshold: f64 = 0.8;

pub fn find_candidates(existing: &[Entry], incoming: &[Entry]) -> Vec<CandidateMatch> {
	let mut candidates = Vec::new();
	for entry_b in incoming {
		for entry_a in existing {
			let similarity = jaccard(&entry_a.minhash, &entry_b.minhash);
			if similarity >= similarity_threshold {
				candidates.push(CandidateMatch {
					entry_a: entry_a.id,
					entry_b: entry_b.id,
					similarity,
					neighbor_corroborated: false,
				});
			}
		}
	}
	candidates
}

pub fn corroborate_neighbors(
	candidates: &mut [CandidateMatch],
	existing: &[Entry],
	incoming: &[Entry],
) {
	let existing_index = |id: EntryId| existing.iter().position(|entry| entry.id == id);
	let incoming_index = |id: EntryId| incoming.iter().position(|entry| entry.id == id);

	let matched_pairs: Vec<(EntryId, EntryId)> = candidates
		.iter()
		.map(|candidate| (candidate.entry_a, candidate.entry_b))
		.collect();

	for candidate in candidates.iter_mut() {
		let position_a = existing_index(candidate.entry_a);
		let position_b = incoming_index(candidate.entry_b);
		let (Some(position_a), Some(position_b)) = (position_a, position_b) else {
			continue;
		};

		let has_neighbor_match = |offset: isize| -> bool {
			let neighbor_a = position_a.checked_add_signed(offset);
			let neighbor_b = position_b.checked_add_signed(offset);
			match (neighbor_a, neighbor_b) {
				(Some(neighbor_a), Some(neighbor_b)) => {
					let id_a = existing.get(neighbor_a).map(|entry| entry.id);
					let id_b = incoming.get(neighbor_b).map(|entry| entry.id);
					match (id_a, id_b) {
						(Some(id_a), Some(id_b)) => matched_pairs
							.iter()
							.any(|(a, b)| *a == id_a && *b == id_b),
						_ => false,
					}
				}
				_ => false,
			}
		};

		candidate.neighbor_corroborated = has_neighbor_match(-1) || has_neighbor_match(1);
	}
}

pub fn filter_candidates(candidates: &[CandidateMatch], existing: &[Entry]) -> Vec<CandidateMatch> {
	candidates
		.iter()
		.filter(|candidate| {
			let entry = existing
				.iter()
				.find(|entry| entry.id == candidate.entry_a);
			match entry {
				Some(entry) if is_short_entry(&entry.body) => candidate.neighbor_corroborated,
				_ => true,
			}
		})
		.cloned()
		.collect()
}

pub fn decide_merge(
	candidate: &CandidateMatch,
	existing: &Entry,
	incoming: &Entry,
) -> MergeDecision {
	if existing.is_quote && existing.is_contaminated && !incoming.is_contaminated {
		return MergeDecision::ReplaceWithNew;
	}
	if incoming.is_quote && incoming.is_contaminated && !existing.is_contaminated {
		return MergeDecision::KeepExisting;
	}
	MergeDecision::KeepExisting
}

pub fn merge_incremental(existing: &mut Document, incoming: &Document) {
	let mut candidates = find_candidates(&existing.entries, &incoming.entries);
	corroborate_neighbors(&mut candidates, &existing.entries, &incoming.entries);
	let confirmed = filter_candidates(&candidates, &existing.entries);

	let matched_b_ids: Vec<EntryId> = confirmed.iter().map(|c| c.entry_b).collect();

	for candidate in &confirmed {
		let existing_entry = existing
			.entries
			.iter_mut()
			.find(|entry| entry.id == candidate.entry_a);
		let incoming_entry = incoming
			.entries
			.iter()
			.find(|entry| entry.id == candidate.entry_b);

		if let (Some(existing_entry), Some(incoming_entry)) = (existing_entry, incoming_entry) {
			match decide_merge(candidate, existing_entry, incoming_entry) {
				MergeDecision::ReplaceWithNew => {
					*existing_entry = incoming_entry.clone();
				}
				MergeDecision::KeepExisting => {}
				MergeDecision::New => {}
			}
		}
	}

	let mut insert_after = existing.entries.len().saturating_sub(1);
	for incoming_entry in &incoming.entries {
		if matched_b_ids.contains(&incoming_entry.id) {
			let position = existing
				.entries
				.iter()
				.position(|entry| {
					confirmed.iter().any(|candidate| {
						candidate.entry_a == entry.id && candidate.entry_b == incoming_entry.id
					})
				});
			if let Some(position) = position {
				insert_after = position;
			}
		} else {
			insert_after += 1;
			let clamped = insert_after.min(existing.entries.len());
			existing.entries.insert(clamped, incoming_entry.clone());
		}
	}
}
