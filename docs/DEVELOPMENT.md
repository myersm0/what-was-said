# what-was-said Development Guide

Personal knowledge base for clipped documents with full-text and semantic search.

## Architecture Overview

```
┌──────────────────────────────────────────────────────────────┐
│                          main.rs                              │
│                     (CLI via clap, dispatch)                  │
└──────────────────────────────────────────────────────────────┘
       │              │              │              │           │
       ▼              ▼              ▼              ▼           ▼
┌───────────┐  ┌───────────┐  ┌───────────┐  ┌───────────┐  ┌───────────┐
│  ingest   │  │  query    │  │  derive   │  │   tui/    │  │   serve   │
│(parse, or-│  │(group +   │  │  (LLM)    │  │(ratatui)  │  │  (axum)   │
│chestrate  │  │rank)      │  └───────────┘  └───────────┘  └───────────┘
└───────────┘  └───────────┘  ┌───────────┐
       │             │        │  extract  │
       ▼             ▼        │  (LLM)    │
┌───────────┐  ┌───────────┐  └───────────┘  ┌───────────┐
│ chunking  │  │ storage/  │                 │  prompts  │
└───────────┘  │ (sqlite)  │                 └───────────┘
               └───────────┘  ┌───────────┐
                              │   util    │
               ┌───────────┐  └───────────┘
               │   llm     │◄─────┐      ┌───────────┐
               │  (trait)  │◄──┐  │      │  openai   │
               └───────────┘   │  │      │  (impl)   │
                          ┌────┘  │      └───────────┘
                    ┌───────────┐ │
                    │  ollama   │─┘
                    │  (impl)   │
                    └───────────┘
```

`LlmBackend` is used by derive, extract, diff, tui, and serve. Ingestion is deterministic by default — no LLM dependency for parsing, chunking, dup detection, or storage. The one exception is the interactive near-duplicate prompt's optional `[v]iew LLM summary` action, which is on demand only and degrades gracefully when no backend is configured (see ingest.rs).

`query` sits between storage and the UI layers (tui, serve, CLI). It owns search result grouping, dedup, and ranking. Both tui and serve call the same query functions.

`prompts` is used by derive, extract, and diff for prompt construction, and by config for prompt hashing.

## Data Model

### Hierarchy

```
Document (1) ──► Entry (n) ──► Chunk (n)
    │               │              │
    │               │              └── vec_chunks (1:1, via sqlite-vec)
    │               │
    │               └── author, timestamp, heading
    │
    ├── source_title, doctype, merge_strategy, tags, derived_content, document_minhash
    │
    ├── claims (n) ──► vec_claims (1:1, via sqlite-vec)
    │
    └── document_relations (n) ──► another Document (near-duplicate pairs, with diff summary)
```

### Key Tables

**documents**: Top-level container. One per source (e.g., one Slack channel conversation, one email thread).
- `source_title`: Browser window title or filename
- `doctype`: Matched type (slack, email, markdown, etc.)
- `merge_strategy`: none | positional (for conversations that grow over time)
- `document_minhash`: MinHash signature over full document body, used for near-duplicate detection at ingest

**entries**: Logical segments within a document (messages, paragraphs, sections). Cascades on document deletion.
- `position`: Order within document
- `author`, `timestamp`: For conversations
- `heading_title`, `heading_level`: For structured docs
- `minhash`: Per-entry MinHash signature

**chunks**: Text fragments for search indexing. Entries are split into chunks of ~300 words. Cascades on entry deletion.
- `chunk_index`: Position within entry
- `body`: The text

**chunks_fts**: FTS5 virtual table for full-text search.

**vec_chunks**: sqlite-vec `vec0` virtual table for semantic search. Stores embeddings with cosine distance metric. Created lazily on first `what-was-said embed` with the dimension detected from the embedding model.

**document_tags**: Many-to-many relationship for tagging.

**derived_content**: LLM-generated summaries (brief + detailed) with quality tracking, model provenance, and source hashing for staleness detection.

**claims**: LLM-extracted atomic propositions from documents. Each claim is a self-contained sentence with provenance back to its source document and (optionally) entry. Stores the extraction model and a hash of the prompt text used, enabling incremental re-extraction when the model or prompt changes.
- `content`: The claim text
- `author`: Inherited from the entry when a single author is known
- `model`: Which LLM produced the extraction
- `prompt_hash`: Hash of the prompt text, for staleness detection

**vec_claims**: sqlite-vec `vec0` virtual table for semantic search over claims. Separate from `vec_chunks` because claims and chunks are different semantic objects with different query purposes and re-embedding lifecycles. Created lazily on first `what-was-said embed` after claims exist.

**document_relations**: One row per near-duplicate decision made at ingest, recording the relationship between a newly-ingested document and an existing one. The document-level sibling of the planned `claim_links` table (same cross-reference shape — a pair, a relation type, a score, optional LLM commentary — at a different granularity). Written for every resolution, including auto-supersedes, so it is a complete audit trail of supersession history.
- `from_document_id`, `to_document_id`: The new and existing documents (both `ON DELETE CASCADE`)
- `relation`: Relation type (currently always `near_duplicate`)
- `similarity`: Jaccard similarity at the time of the decision
- `shared_block_words`: Longest shared contiguous block in words; NULL when the pair was auto-superseded on Jaccard alone (the block was never computed — see ingest.rs)
- `resolution`: `superseded` | `kept_both` | `pending` (deferred non-interactive cases), CHECK-constrained
- `summary`, `summary_model`, `summary_prompt_hash`, `summarized_at`: LLM diff summary and its provenance, for staleness detection
- `UNIQUE(from_document_id, to_document_id)`; both endpoints indexed

## Module Responsibilities

### main.rs
CLI parsing via clap derive API and command dispatch. Registers the sqlite-vec extension via `sqlite3_auto_extension`. Contains `open_db()` for connection setup and `create_backend()` factory that returns `Box<dyn LlmBackend>` based on `BackendConfig`.

The `--config` flag specifies the config directory (default: `~/.config/what-was-said/`). `LlmsConfig::load` reads all LLM configuration (backend, auth, model defaults, and per-task overrides) from `llms.toml` within this directory, falling back to the legacy `backend.toml`/`derive.toml`/`extract.toml` files when `llms.toml` is absent. CLI flags (`--backend`, `--ollama`, `--model`, `--embed-model`) are optional overrides; `--model` overrides both the general and diff models. All CLI backend/model flags are `Option<String>` — no hardcoded defaults in clap.

Subcommands: ingest (file or directory, with `--non-interactive`), about (with `--method=[exact|semantic]`), in, dump, stats, embed, derive, extract, diff, serve, browse (default).

Global flags: `--db`, `--config`, `--backend`, `--ollama`, `--model`, `--embed-model`, `--theme`, `--json`.

### llm.rs
`LlmBackend` trait defining the interface for LLM providers:
- `generate(prompt, model, system, format)`: Full generation with optional system prompt and format (e.g., JSON mode)
- `chat(prompt, model)`: Default implementation calling `generate` without system prompt or format
- `embed(text, model)`: Text embedding, returns float vector

The trait requires `Send + Sync` for use in the async serve module.

### ollama.rs
`OllamaClient` implementing `LlmBackend` via the Ollama HTTP API. Constructor takes just `base_url`; the model is passed per-call.

### openai.rs
`OpenAiClient` implementing `LlmBackend` via the OpenAI chat completions and embeddings APIs. Supports two authentication strategies:

- **API key** (`auth = "api_key"`): Reads `OPENAI_API_KEY` from environment. Compatible with standard OpenAI, Azure OpenAI, and other OpenAI-compatible endpoints.
- **OAuth2 client credentials** (`auth = "oauth"`): Reads `OAUTH2_CLIENT_ID` and `OAUTH2_CLIENT_SECRET` from environment. Fetches bearer tokens from a configured token URL with a configured scope. Tokens are cached in a `Mutex<Option<CachedToken>>` (for `Send + Sync` compatibility) and refreshed automatically 60 seconds before expiry.

The primary constructor is `from_config(&OpenAiConfig)`, taking its configuration from the `[openai]` section of `llms.toml`. A convenience `from_env()` constructor remains for the plain API key case.

Translates the `format: Some("json")` parameter to OpenAI's `response_format: {"type": "json_object"}`.

### serve.rs
Axum-based localhost JSON API server. Holds an `AppState` with `Mutex<Connection>` and `Box<dyn LlmBackend>`.

Endpoints:
- `GET /search?q=...&sort=score|date&author=...&date_from=...&date_to=...` — FTS5 search via `query::search_filtered`, results grouped by document
- `GET /similar?q=...&limit=N&author=...&date_from=...&date_to=...` — semantic search via `query::find_similar_grouped_filtered`, results grouped by document
- `GET /get/:id` — full document with entries and chunks
- `GET /entries/:doc_id` — entries for a document
- `GET /claims/doc/:doc_id` — claims extracted from a document
- `GET /claims/similar?q=...&limit=N` — semantic search over claims via `vec_claims`
- `GET /stats` — document/entry/chunk/embedding/claim counts
- `GET /derive/status` — derivation progress

Creates a tokio runtime internally so the rest of the binary stays synchronous.

### query.rs
Shared query layer between storage and the UI layers (tui, serve, CLI). Owns search result grouping, deduplication, and ranking. No `Connection` parameter on the pure domain functions, making them testable with synthetic data.

**Types**:
- `GroupedSearchResult`: Documents with nested `ChunkHit` vectors
- `ChunkHit`: A single search hit within a document
- `SearchSortColumn`: Score or Date
- `SearchFilters`: Author, date_from, date_to — shared by TUI and serve

**Composed functions** (call storage, then group):
- `search()` / `search_filtered()`: FTS5 search via `storage::raw_fts_search`, then group/dedup/sort
- `find_similar_grouped()` / `find_similar_grouped_filtered()`: semantic search via `storage::find_similar_chunks_filtered`, then group/sort
- `strip_fts_markers()`: removes FTS5 snippet highlight control characters from grouped results

**Pure domain functions** (no DB, testable with synthetic row data):
- `group_fts_results()`: groups `ChunkSearchResult` rows by document, deduplicates similar snippets, sorts by score or date
- `group_similar_results()`: groups `SimilarChunk` rows by document, converts similarity to negative rank, sorts by rank
- `snippets_similar()`: word-overlap similarity check for deduplication

### derive.rs
LLM summary generation. Pure orchestration: iterates documents, calls `prompts.rs` for prompt construction, calls `backend.chat()`, stores results.

Key functions:
- `run()`: Iterates documents needing derivation, generates detailed then brief summaries
- `run_status()`: Reports derivation progress
- `derive_detailed()`: Resolves prompt via `config.resolve_detailed_prompt()`, assembles via `prompts::detailed_summary_prompt()`, generates, stores
- `derive_brief()`: Resolves prompt via `config.resolve_brief_prompt()`, assembles via `prompts::brief_summary_prompt()`, or copies detailed directly for short documents (under `short_threshold`)

### extract.rs
LLM claim extraction. Pure orchestration: iterates documents, calls `prompts.rs` for prompt construction, calls `backend.chat()`, parses and stores results.

Key functions:
- `run()`: Selects documents needing extraction (missing claims or stale model/prompt), deletes existing claims, re-extracts, stores results
- `run_status()`: Reports extraction progress (documents with/without claims, total claim count)
- `extract_document()`: Resolves framing and rules from config, assembles prompt via `prompts::claim_extraction_prompt()`, calls LLM, parses response, inserts claims
- `parse_claims()`: Lenient line parser. Strips bullet markers (`- `, `* `), numbered prefixes (`1. `), bracket labels (`[kind]`), and skips blank lines and preamble. Returns clean claim strings.
- `resolve_author()`: If all entries in a document share a single author, inherits it for claims

Staleness detection: each claim stores the `model` and `prompt_hash` that produced it. `get_documents_needing_extraction(model, prompt_hash)` returns documents whose claims don't match the current config, enabling automatic incremental re-extraction when the model or prompt changes.

### prompts.rs
Single source of truth for all prompt construction. Contains default prompt text and assembly functions. No config loading or file I/O — takes resolved values as parameters.

- `LengthTier`: Enum (Short/Medium/Long) with `from_len(content_len, short_threshold, medium_threshold)` and `key()` (returns `"short"`/`"medium"`/`"long"` for config map lookup)
- `detailed_summary_prompt(document_text, instructions)`: Assembles detailed summary prompt
- `brief_summary_prompt(detailed_summary, instructions)`: Assembles brief summary prompt
- `claim_extraction_prompt(document_text, rules, framing)`: Assembles claim extraction prompt
- `document_diff_prompt(added, removed, instructions)`: Assembles the near-duplicate change-summary prompt from the non-overlapping regions of a pair
- `compute_prompt_hash(rules)`: Hashes rules text for staleness detection
- `default_detailed_prompt(tier)`, `default_brief_prompt()`, `default_extract_rules()`, `default_diff_instructions()`: Built-in fallback prompt text

### config.rs
Loads and parses configuration files from the config directory. Handles doctype detection.

**Doctype matching** (in order):
1. `source_pattern` regex against source title
2. `extension` match against file extension
3. Content sniffing (markdown headers, copilot email format)

Key types:
- `Doctype`: Parsed config entry
- `DoctypeMatch`: Result of detection, includes parser/preprocessor/merge_strategy
- `TagConfig`: Tag hierarchy, default exclusions, color assignments
- `DeriveConfig`: Model selection, prompt thresholds. `resolve_detailed_prompt(content_len)` checks the prompts map by tier key (`"short"`, `"medium"`, `"long"`), then `"default"`, falling back to built-in defaults via `prompts.rs`. `resolve_brief_prompt()` same pattern.
- `ExtractConfig`: Model selection, source-format framing paths, rules path. `prompt_hash()` delegates to `prompts::compute_prompt_hash()`.
- `DiffConfig`: Model selection for near-duplicate change summaries.
- `BackendConfig`: Backend selection, model defaults, OpenAI auth configuration
- `BackendKind`: Enum (`Ollama`, `OpenAi`)
- `OpenAiAuth`: Enum (`ApiKey`, `OAuth`)
- `OpenAiConfig`: Base URL, auth strategy, OAuth token URL and scope
- `LlmsConfig`: Aggregate of `BackendConfig`, `DeriveConfig`, `ExtractConfig`, and `DiffConfig`, loaded from a single `llms.toml`

`LlmsConfig::load(config_dir)` reads `llms.toml` from the config directory and builds all four sub-configs. The top-level `model` is the default generation model; each per-task model (`[derive].detailed_model`/`brief_model`, `[extract].model`, `[diff].model`) falls back to it when omitted, and to a compiled-in default (`qwen2.5:32b` for generation, `gemma3:27b` for extraction) when the top-level is also unset — so the diff/`[v]` path can never silently fall through to an unrelated default. If `llms.toml` is absent, `load_legacy` reads the older `backend.toml`/`derive.toml`/`extract.toml` via the still-present `BackendConfig::load`/`DeriveConfig::load`/`ExtractConfig::load`, and resolves the diff model from the backend model. CLI `--model` overrides both the backend and diff models.

### ingest.rs
Text parsing, segmentation, and ingestion orchestration. No LLM dependency — ingestion is a pure function of (file + config).

**Parsers**:
- `Whole`: Entire file as single entry
- `Markdown`: Split on headings
- `CopilotEmail`: Parse Copilot-formatted email threads
- `Whisper`: Declared but not yet implemented

**External preprocessors**: Python scripts that return JSON. Called via:
```rust
run_preprocessor(script_path, file_path) -> SegmentationResult
```

**Orchestration**:
- `ingest_file()`: Main ingestion logic — reads file, detects doctype, parses, handles merge, detects near-duplicates, stores results
- `ingest_directory()`: Iterates directory, calls ingest_file per file
- `find_overlap()`: Detects overlapping entries for positional merge

**Near-duplicate detection**: After parsing but before inserting, computes a document-level MinHash signature and compares against existing documents within a ±180 day window (`dup_window_days`). The decision combines two signals: Jaccard similarity over the MinHash signatures, and `minhash::longest_shared_block_words` (the longest contiguous run of shared shingles, in words). Jaccard ≥ `dup_jaccard_high` (0.7) or a shared block ≥ `dup_shared_block_words` (300) auto-supersedes the older document. Jaccard between `dup_jaccard_low` (0.4) and `dup_jaccard_high` with a smaller block is the gray zone, resolved interactively (or kept-both under `--non-interactive`). Below `dup_jaccard_low` is not a match.

The candidate loop breaks on the first auto-supersede, so when a pair clears the Jaccard-high bar the shared-block computation is skipped and `shared_block_words` is recorded as NULL — Jaccard runs on the stored signatures (no document text), whereas the block requires fetching the existing document's text and running an O(m·n) LCS, which is deliberately avoided once the decision is already made. Making the block always-computed (for a uniformly-populated audit trail) is a ~5-line reorder, left as a future option.

**Interactive resolution**: `prompt_gray_zone` presents `[s]upersede old / [k]eep both / [d]iff / [v]iew LLM summary / [q]uit` on stderr/stdin. Interactive is the default for both single-file and directory ingests; `--non-interactive` keeps both and emits an end-of-run review summary. `[q]uit` aborts the whole run, so `ingest_file` returns an `IngestOutcome` enum (`Ingested` / `Skipped` / `Quit`) rather than a bool. `[v]` calls the (optional) backend on demand to summarize the change; the backend is passed as `Option<&dyn LlmBackend>` so ingestion still runs with no LLM configured. Prompts and the summary identify documents by file path.

**Relation recording**: every decision (including auto-supersedes and non-interactive deferrals) is written to `document_relations` in the same transaction as the new document, via `storage::insert_document_relation`. A summary generated by `[v]` is stored on the row at resolution time.

### storage/
All SQLite operations. Uses rusqlite directly (no ORM). Integrates sqlite-vec for vector search. Split into submodules, all re-exported from `storage/mod.rs`. Sets `PRAGMA foreign_keys = ON` and `PRAGMA journal_mode=WAL` on initialization for referential integrity and concurrent read access.

Key output types (`SimilarChunk`, `SimilarClaim`, `Claim`, `DumpDocument`, `DumpEntry`, `DocumentContent`, `EntryContent`, `ChunkContent`, `DeriveStatus`) derive `Serialize` for JSON output. Search result grouping types (`GroupedSearchResult`, `ChunkHit`, `SearchSortColumn`) live in `query.rs`.

**mod.rs**: Schema initialization (`initialize()`), foreign keys pragma, WAL mode pragma, `migrate()` for schema upgrades, re-exports, tests.

`migrate()` runs on every startup and handles: rebuilding the entries table to add `ON DELETE CASCADE` if missing, adding the `document_minhash` column to documents if missing, migrating the claims table if it has the old `kind` column or lacks `prompt_hash` (drops and recreates the table; re-extract to repopulate), creating the `document_relations` table and its indexes if absent (also defensively `ALTER`-ing in any summary columns missing from an older copy), and cleaning up any orphaned entries/chunks with an FTS rebuild.

**documents.rs**: Document/entry/chunk CRUD, list/get/dump, counts, merge helpers.
- `insert_document/entry/chunks`: Write path
- `get_document()`: Read full document with entries and chunks
- `list_documents()`: Browse-mode listing with brief summaries and tags
- `find_documents_by_merge_key()`: Finds candidates for positional merge
- `find_dup_candidates()`: Finds documents within a time window that have MinHash signatures, for near-duplicate comparison
- `insert_document_relation()`: Records a near-duplicate decision in `document_relations`
- `set_relation_summary()`: Stores an LLM diff summary (with model + prompt hash) on a relation
- `get_relations_needing_summary(model, prompt_hash)`: Relations with a missing or stale summary, for the `diff` command

**search.rs**: Raw FTS5 database access. No grouping or ranking — those live in `query.rs`.
- `raw_fts_search()`: FTS5 MATCH with snippet generation, author/date filters pushed into SQL. Returns flat `Vec<ChunkSearchResult>`.

**embed.rs**: sqlite-vec integration for both chunks and claims.
- `ensure_vec_table()`: Creates `vec_chunks` vec0 virtual table with detected embedding dimension
- `insert_embedding()`: Writes chunk embedding via zerocopy
- `find_similar_chunks_filtered()`: KNN search via sqlite-vec `MATCH` with cosine distance
- `ensure_vec_claims_table()`: Creates `vec_claims` vec0 virtual table, separate from `vec_chunks`
- `insert_claim_embedding()`: Writes claim embedding
- `find_similar_claims()`: KNN search over claims, joining back to `claims` and `documents` for metadata
- `get_claims_without_embeddings()`: Claims needing embedding
- `count_claims_with/without_embeddings()`: Progress tracking

**tags.rs**: Tag add/remove/list/get operations.

**derive.rs**: Derived content CRUD, derive status, source hash computation for staleness detection.

**claims.rs**: Claim CRUD, extraction status, staleness-aware document selection.
- `insert_claim()`: Write path (document_id, entry_id, author, content, model, prompt_hash)
- `get_claims_for_document()`: All claims for a document, ordered by id
- `delete_claims_for_document()`: Bulk delete before re-extraction
- `get_documents_needing_extraction(model, prompt_hash)`: Returns document ids where no claims match both the current model and prompt hash — handles missing claims, model changes, and prompt changes in a single query

### util.rs
Shared string utilities.

- `strip_source_suffix()`: Removes browser names, URLs from source titles. Used for both merge key matching and TUI group navigation.
- `extract_group_key()`: Derives a grouping key from a source title by stripping suffixes and filtering out generic names. Used for multi-document navigation in the TUI read view.
- `normalize_to_ascii()`: Converts curly quotes, em-dashes, ellipsis to ASCII equivalents. Collapses whitespace (including non-breaking spaces).
- `truncate_str()`: Char-boundary-safe string truncation.
- `strip_fts_markers()`: Removes FTS5 snippet highlight control characters (`\x01`, `\x02`, `\x03`) for clean JSON output.
- `diff_regions(new, existing)`: Line-level set difference returning the only-in-new and only-in-existing blocks. Used by the ingest `[d]iff` view and to feed the diff summary prompt only the changed regions.

### chunking.rs
Splits entry text into chunks for indexing.

Strategy: Sliding window of ~300 words with 1/3 stride. Snaps boundaries to sentence ends and paragraph breaks within a max distance of 500 chars; falls back to word boundaries when no suitable boundary is nearby. Entries under 400 words are kept as a single chunk. Remaining words below the split threshold are absorbed into the final chunk to avoid tiny trailing fragments.

Sentence boundaries are detected by terminal punctuation (`.!?` followed by whitespace) and paragraph breaks (`\n\n`, with optional intermediate whitespace). This ensures documents without terminal punctuation (lists, notes, bullet points) still get chunked at natural paragraph boundaries rather than producing oversized single chunks.

### tui/
Ratatui-based terminal UI. Split into submodules with a shared `App` struct in `mod.rs`. Each mode has a key handler and draw function; the event loop dispatches based on `app.mode`.

**mod.rs**: App struct (all state), enums (Mode, SearchMode, SearchField, SummaryType), shared methods (load_documents, filtered_documents, load_document_for_reading, navigate_group, etc.), `run()`/`run_app()` event loop, `draw()` dispatcher, `entry_chunk_count()`/`document_chunk_count()` helpers. `SearchConfig` holds a `&dyn LlmBackend` reference and `embed_model` string.

**theme.rs**: Theme system. `Theme` struct with 19 semantic color slots (background, text hierarchy, borders, highlights, etc.), hex (`#RRGGBB`) and named color parsing, TOML loading. Five built-in themes compiled via `include_str!` (dracula, gruvbox, nord, solarized, light). Resolution order: builtin name → absolute path → config dir themes folder → fallback to dracula.

**browse.rs**: Browse mode — document list with sorting, filtering, tag color markers, brief summary preview. Centered vertical scrolling.

**read.rs**: Read mode — view document content, navigate chunks, yank to clipboard, group navigation. Skips empty-body entries during chunk navigation.

**search.rs**: Search mode — FTS5 or semantic search (semantic by default) with author/date filters, backtick mode toggle, search execution. Builds a `SearchFilters` from app state and calls `query::search_filtered()` or `query::find_similar_grouped_filtered()`. Uses `search_config.backend.embed()` for semantic queries.

**tags.rs**: TagEdit and TagFilter modes — add/remove tags, filter document list by tag.

**summary.rs**: SummaryView mode — popup for viewing/toggling brief/detailed summaries, copy, mark bad.

**render.rs**: Shared rendering — status bar with key binding highlighting, markdown line/inline parsing, table alignment, snippet parsing with match highlighting, centered rect utility.

### types.rs
Shared type definitions: `DocumentId`, `EntryId`, `MediaId`, `SegmentedEntry`, `SegmentationResult`, `MergeStrategy`, `MediaItem`, `MediaType`, `MinHashSignature`.

### minhash.rs
MinHash signatures for near-duplicate detection. Tokenizes text into 3-word shingles (sliding windows of 3 consecutive words) for order-sensitive fingerprinting. Texts shorter than 3 words fall back to individual token hashing.

Key functions:
- `minhash()`: Compute a MinHash signature for arbitrary text
- `minhash_document()`: Compute a document-level signature by concatenating all entry bodies
- `minhash_with_context()`: Compute with optional surrounding entry text for contextual hashing
- `jaccard()`: Estimate Jaccard similarity between two signatures
- `longest_shared_block_words()`: Longest contiguous block of shared 3-word shingles between two texts, in approximate words (rolling-DP longest-common-substring over interned shingle ids). The structural signal for near-duplicate detection, catching revisions that share a large block but fall below the Jaccard threshold overall.
- `is_short_entry()`: Check if text is below 50 chars (used for short-entry handling)

### diff.rs
The `what-was-said diff` command: batch generation of LLM change summaries for near-duplicate pairs. Walks relations whose summary is missing or stale (`get_relations_needing_summary`, or all relations under `--force`), fetches both documents' text, computes the non-overlapping regions via `util::diff_regions`, assembles the prompt via `prompts::document_diff_prompt`, calls `backend.chat()`, and stores the result with the model and prompt hash via `storage::set_relation_summary`. Complements the on-demand `[v]` summary at ingest, covering relations that were never viewed interactively (notably non-interactive `pending` ones).

### markdown.rs
Markdown-specific parsing (heading extraction, section splitting). Tracks fenced code block state (```` ``` ```` and `~~~` delimiters) so that `#` comment lines inside code blocks are not misinterpreted as headings.

### whisper.rs
VTT transcript parsing for Whisper output. Parses segments and converts to `MediaItem` list. Not yet wired into the ingestion pipeline.

## Configuration

### config.toml

```toml
[[doctype]]
name = "slack"
source_pattern = "(Channel|DM).*Slack"
parser = "whole"
merge_strategy = "positional"
preprocessor = "~/.config/what-was-said/parsers/slack_parser.py"
skip = false
merge_consecutive_same_author = true
cleanup_patterns = ["^\\s*:\\w+:\\s*$"]
```

**Fields**:
- `name`: Identifier for this doctype
- `source_pattern`: Regex matched against source title
- `extension`: Alternative match by file extension
- `parser`: whole | markdown | whisper | copilot_email
- `merge_strategy`: none | positional
- `preprocessor`: Path to external parser script (~ expanded)
- `skip`: If true, files matching this doctype are skipped
- `cleanup_patterns`: Regexes for lines to remove
- `merge_consecutive_same_author`: Combine adjacent same-author entries

### llms.toml

All LLM configuration in one file: backend selection, authentication, default models, and per-task overrides. Replaces the legacy `backend.toml`/`derive.toml`/`extract.toml`; if `llms.toml` is absent those three are read as a fallback.

```toml
backend = "openai"
ollama_url = "http://localhost:11434"
model = "gpt-4.1-mini"            # default generation model for every task
embed_model = "text-embedding-3-small"

[openai]
base_url = "https://api.openai.com/v1"
auth = "oauth"
oauth_token_url = "https://login.microsoftonline.com/.../oauth2/v2.0/token"
oauth_scope = "api://.../.default"

[derive]
detailed_model = "qwen2.5:32b"
brief_model = "qwen2.5:32b"
prompt_version = "v1"
short_threshold = 1200
medium_threshold = 3500
[derive.prompts]
short = "~/.config/what-was-said/prompts/detailed_short.txt"
medium = "~/.config/what-was-said/prompts/detailed_medium.txt"
long = "~/.config/what-was-said/prompts/detailed_long.txt"
brief = "~/.config/what-was-said/prompts/brief.txt"

[extract]
model = "gemma3:27b"
rules = "~/.config/what-was-said/prompts/extract_rules.txt"
[extract.framings]
email = "~/.config/what-was-said/prompts/extract_framing_conversation.txt"
slack = "~/.config/what-was-said/prompts/extract_framing_conversation.txt"
whisper = "~/.config/what-was-said/prompts/extract_framing_voice.txt"

[diff]
model = "qwen2.5:32b"
```

**Top-level fields**:
- `backend`: `ollama` (default) or `openai`
- `ollama_url`: Ollama endpoint (default: `http://localhost:11434`)
- `model`: Default generation model. Every per-task model falls back to this when its section (or its `model` key) is omitted; the final fallback when this is also unset is a compiled-in default (`qwen2.5:32b` for generation, `gemma3:27b` for extraction). Overridden by `--model`, which also overrides the diff model.
- `embed_model`: Default embeddings model (overridden by `--embed-model`)

**`[openai]` section**:
- `base_url`: API endpoint (default: `https://api.openai.com/v1`)
- `auth`: `api_key` (default) or `oauth`
- `oauth_token_url`, `oauth_scope`: Required when `auth = "oauth"`
- Env vars: `OPENAI_API_KEY` for `api_key`; `OAUTH2_CLIENT_ID` / `OAUTH2_CLIENT_SECRET` for `oauth`

**`[derive]` section**: Summary generation. Prompt tier is selected by document content length: short (<`short_threshold`), medium (<`medium_threshold`), long (≥`medium_threshold`). Custom prompts under `[derive.prompts]` are resolved by tier key (`"short"`/`"medium"`/`"long"`), then `"default"`, falling back to built-in defaults. For short documents, the brief summary is copied from the detailed output without an extra LLM call.

**`[extract]` section**: Claim extraction. `rules` is an optional path to a custom extraction-rules prompt (compiled-in adaptive prompt if absent). `[extract.framings]` are optional per-doctype structural hints (e.g. "attribute to speaker"), keyed by doctype name from `config.toml` — not domain instructions, which the adaptive prompt handles.

**`[diff]` section**: Near-duplicate change summaries. Only `model` for now; the diff instructions are the compiled-in default (a custom prompt path, mirroring `[extract].rules`, is a future addition).

All sections are optional. With no `llms.toml` and no legacy files, the system defaults to Ollama on localhost with the compiled-in per-task model defaults.

### tags.toml

```toml
[defaults]
exclude = ["junk", "archived"]

[includes]
project-x = ["x-frontend", "x-backend", "x-infra"]

[colors]
research = "cyan"
project = "green"
reference = "blue"
```

- `[defaults].exclude`: Tags filtered out by default (override with `--include-all`)
- `[includes]`: Parent tags that match documents tagged with any child
- `[colors]`: Tag color for browse view markers (named colors or hex `#RRGGBB`)

### derive.toml / extract.toml (legacy)

Read only when `llms.toml` is absent. Same fields as the `[derive]` and `[extract]` sections above, but as standalone top-level files (no section header). Kept for backward compatibility; new setups should use `llms.toml`.

## External Preprocessors

Python scripts that parse format-specific content.

**Contract**:
- Input: File path as CLI argument
- Output: JSON to stdout

```json
{
  "entries": [
    {
      "body": "Message text",
      "author": "Jane Smith",
      "timestamp": "2024-01-15T10:30:00",
      "heading_title": null,
      "heading_level": null
    }
  ]
}
```

Only `body` is required. Timestamps should be ISO 8601, normalized to UTC.

**Invocation**: `python3 script.py /path/to/file.txt`

See `examples/parsers/` for working email and Slack preprocessor scripts, and `examples/inbox/` for sample input documents.

## Ingestion Flow

```
1. Read file
2. Extract "# source: ..." header line → source_title
3. Match doctype (config.detect_with_content)
4. If skip=true, return early
5. If preprocessor defined, call it; else use built-in parser
6. Apply cleanup_patterns
7. If merge_consecutive_same_author, combine entries
8. Normalize source_title to ASCII, compute merge key via strip_source_suffix
9. If merge_strategy=positional:
   a. Find existing docs with same merge_key
   b. Check each for overlapping entries (≥150 chars consecutive match)
   c. If overlap found, append new entries to existing doc
   d. Else fall through to step 10
10. Compute document-level MinHash signature
11. Query existing docs within ±180 day window for near-duplicate candidates
12. For each candidate, evaluate Jaccard and (in the gray band) longest shared block:
    a. Jaccard ≥ 0.7 or shared block ≥ 300 words → auto-supersede older doc
    b. 0.4 ≤ Jaccard < 0.7 with smaller block → resolve interactively (or keep both if --non-interactive)
    c. Jaccard < 0.4 → not a match
13. Insert document/entries/chunks
14. Record each near-duplicate decision in document_relations (same transaction)
15. Index in FTS5 (via trigger)
```

(`[q]uit` at an interactive prompt aborts the whole run; a `[v]iew` summary generated during resolution is stored on the relation row in step 14.)

## Search Flow

### FTS5
```
storage/search.rs (raw DB access):
1. Convert query to prefix search ("foo bar" → "foo* bar*")
2. Build SQL with author/date filters in WHERE clause
3. Execute FTS5 MATCH query with snippet()
4. Return flat Vec<ChunkSearchResult>

query.rs (grouping/ranking):
5. Group results by document_id
6. Deduplicate similar snippets within document
7. Sort by best rank (or date)
```

### Semantic
```
storage/embed.rs (raw DB access):
1. Embed query text via LlmBackend (default: qwen3-embedding:8b via Ollama)
2. Run KNN query against vec_chunks using sqlite-vec MATCH
3. Join results back to chunks/entries for metadata
4. Filter by author/date if specified
5. Return flat Vec<SimilarChunk>

query.rs (grouping/ranking):
6. Group by document_id
7. Convert similarity to negative rank
8. Sort by best rank
```

## Embeddings

Chunk embeddings are stored in `vec_chunks`, claim embeddings in `vec_claims` — both sqlite-vec `vec0` virtual tables with cosine distance metric. Tables are created lazily on the first `what-was-said embed` run, with the embedding dimension detected from the model's response.

**Generate**: `what-was-said embed [--limit N] [--embed-model MODEL]`

The embed command processes chunks first, then claims, with separate progress reporting for each.

**Default model**: qwen3-embedding:8b via Ollama

**Search**: KNN via sqlite-vec's `WHERE embedding MATCH ? AND k = ?` syntax. Sublinear in collection size. Chunk search and claim search are independent queries against their respective tables.

## Key Design Decisions

1. **Chunk-level search, document-level display**: Search indexes chunks for precision, but results are grouped by document for context. Grouping, deduplication, and ranking logic lives in `query.rs`, shared by TUI, serve, and CLI.

2. **Positional merge**: Conversation documents (Slack, email) grow over time. Overlap detection allows appending new messages without duplicating.

3. **External preprocessors**: Format-specific parsing is delegated to Python scripts. Easier to iterate on parsing logic without recompiling.

4. **No ORM**: Direct rusqlite for simplicity and control. Schema is simple enough that an ORM adds more complexity than it removes.

5. **sqlite-vec for embeddings**: KNN search via vec0 virtual tables replaces brute-force cosine similarity. Scales to large collections without loading all embeddings into memory.

6. **Short-doc brief optimization**: Documents under the short threshold get their brief summary copied from the detailed output, saving an LLM round-trip on content that's already 1-2 sentences.

7. **LLM backend abstraction**: `LlmBackend` trait with `OllamaClient` and `OpenAiClient` implementations. Backend selection and model defaults are configured in `llms.toml`, with CLI flags as overrides. `OpenAiClient` supports both static API keys and OAuth2 client credentials with automatic token refresh, enabling use on machines with institutional OpenAI endpoints.

8. **WAL mode**: SQLite journal_mode=WAL set on initialization. Enables concurrent readers during serve mode without blocking on writes.

9. **JSON API via serve**: Localhost axum server holding the DB connection open, exposing query functions as JSON endpoints. Designed as the agent-facing interface, avoiding per-call process spawn overhead.

10. **Foreign key enforcement**: `PRAGMA foreign_keys = ON` set on every connection. All parent-child relationships use `ON DELETE CASCADE`. A migration system in `initialize()` detects old schemas and rebuilds tables as needed, preventing orphaned rows.

11. **Near-duplicate detection via two signals**: Documents are fingerprinted at ingest using 3-word shingle MinHash signatures and compared against existing docs within a configurable window (default ±180 days). The decision combines Jaccard similarity with a longest-shared-block heuristic — the latter catches revisions that share a large contiguous block but fall below the Jaccard threshold overall (clip → edit → re-clip workflows). Strong matches auto-supersede the older version (tagging it `superseded`, preserving both); a middle band is resolved by an interactive prompt, defaulting on rather than logging to stderr. The block is intentionally not computed when a pair is auto-superseded on Jaccard alone (it would cost a text fetch plus an O(m·n) LCS the decision doesn't need), so `shared_block_words` is NULL in that case.

12. **Themeable TUI via semantic color slots**: All TUI colors are driven by a `Theme` struct with named semantic slots (background, text, text_muted, highlight_bg, heading, code, etc.) rather than hardcoded terminal colors. Themes are TOML files with hex color values. Five built-in themes are compiled into the binary; custom themes can be loaded from the config directory or an absolute path. Tag colors remain user-configured independently of the theme.

13. **Paragraph-aware chunking**: Chunk boundary detection recognizes both sentence-ending punctuation (`.!?`) and paragraph breaks (`\n\n`) as snap points. This prevents documents without terminal punctuation (bullet lists, notes, code-heavy content) from producing oversized single chunks.

14. **Config-directory convention**: All configuration files (`config.toml`, `llms.toml`, `tags.toml`, themes) live in a single config directory (default `~/.config/what-was-said/`), overridable with `--config`. This allows multiple configurations (e.g. personal vs. work) by pointing at different directories.

15. **Claim extraction with adaptive prompt**: A single prompt handles all document types. The LLM adapts its extraction to whatever is substantive in the text rather than following per-domain rules. Source-format framings (email attribution, voice memo context) are retained as structural hints only. No stored classification — testing showed models are weak at it, and the claim text itself is the representation.

16. **Incremental re-extraction via model + prompt_hash**: Each claim stores the model and a hash of the prompt rules that produced it. `what-was-said extract` compares against the current config and re-extracts only stale documents. This means changing the configured model or editing the prompt file triggers automatic re-extraction without `--force`.

17. **Separate vec_claims table**: Claims and chunks are different semantic objects — claims are atomic propositions, chunks are text fragments. They have different query purposes (what was said vs. where is it) and different re-embedding lifecycles (re-extract claims without re-chunking). Mixing them in one vector table would couple these concerns.

18. **Query layer between storage and UI**: `query.rs` owns all search result grouping, deduplication, and ranking. TUI, serve, and CLI all call the same functions, preventing divergence. Raw DB access in `storage/search.rs` returns flat rows; `query.rs` composes the pipeline. The grouping functions are testable with synthetic data (no DB needed).

19. **Centralized prompt construction**: All prompt assembly lives in `prompts.rs`. Derive and extract are pure orchestration — they call prompt functions, then call the LLM, then store results. Custom prompts are resolved by config (per-tier keys for derive, rules/framings for extract), with built-in defaults compiled into the binary. `compute_prompt_hash` lives here too, breaking what was previously a circular dependency between config and extract.

20. **Deterministic ingestion (with one scoped exception)**: Ingestion has no LLM dependency for parsing, chunking, dup detection, or storage — all deterministic (built-in parsers or external preprocessor scripts), with LLM enrichment (summaries, embeddings, claims) as a separate post-ingest step. The single exception is the interactive near-duplicate prompt's `[v]iew LLM summary` action: it calls the LLM on demand only, takes the backend as `Option<&dyn LlmBackend>`, and degrades to "no backend configured" so core ingestion always runs LLM-free.

21. **Shared-block heuristic at shingle granularity**: The longest-shared-block signal is computed over interned 3-word shingle ids (a rolling-DP longest-common-substring), not characters. The question is only "is there a big shared block?", so shingle granularity is sufficient and reuses the existing tokenizer; the result is reported as an approximate word count. The full block computation is gated behind the gray band precisely because it is the expensive part (text fetch + O(m·n) LCS), unlike Jaccard which runs on the stored signatures.

22. **Persisted relations as the document-level link table**: Every near-duplicate decision is recorded in `document_relations`, the document-level sibling of the planned `claim_links` table (same pair/type/score/commentary shape at a different granularity). Recording all decisions — including auto-supersedes and non-interactive deferrals — turns near-dup handling into a queryable audit trail rather than ephemeral stderr output, and gives the diff summaries (at-ingest `[v]` and batch `diff`) a durable place to attach, keyed for staleness by model + prompt hash like derive/extract.

23. **All LLM config in one llms.toml**: Backend, auth, default models, and per-task (`[derive]`/`[extract]`/`[diff]`) overrides live in a single file, with each per-task model falling back to a top-level default. This removes the footgun where a task without its own configured model silently fell through to an unrelated global default. Legacy `backend.toml`/`derive.toml`/`extract.toml` are still read when `llms.toml` is absent, so migration is optional.

## Adding a New CLI Command

1. Add a variant to the `Command` enum in main.rs with clap attributes
2. Implement logic in the appropriate module (or a new one)
3. Add a match arm in `main()`
4. Add the module to `lib.rs` if new

## Adding a New LLM Backend

1. Create a new module (e.g., `anthropic.rs`)
2. Implement `LlmBackend` for your client struct
3. Add a variant to `BackendKind` in config.rs and handle it in `BackendConfig` deserialization
4. Add a case to `create_backend()` in main.rs
5. Add the module to `lib.rs`

## Common Maintenance Tasks

**Reset database**: Delete `~/.local/share/what-was-said/what-was-said.db`

**Re-embed everything**: `DROP TABLE vec_chunks; DROP TABLE IF EXISTS vec_claims;` in sqlite3, then `what-was-said embed`

**Re-extract all claims**: `what-was-said extract --force` then `what-was-said embed`

**Re-summarize near-duplicate changes**: `what-was-said diff --force`

**Inspect near-duplicate relations**: `SELECT from_document_id, to_document_id, resolution, similarity, shared_block_words FROM document_relations ORDER BY similarity DESC;` in sqlite3

**Debug ingestion**: Run with a single file directly. Gray-zone near-duplicates prompt interactively; pass `--non-interactive` for an unattended run with an end-of-run review summary of deferred cases.

**Profile search**: Add timing around `query::search()` or `query::find_similar_grouped()`

**Run tests**: `cargo test` (all tests use in-memory SQLite, no network or filesystem dependencies)
