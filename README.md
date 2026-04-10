# what-was-said

[![CI](https://github.com/myersm0/what-was-said/actions/workflows/ci.yml/badge.svg)](https://github.com/myersm0/what-was-said/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/myersm0/what-was-said)](https://github.com/myersm0/what-was-said/releases/latest)

A system for collecting, structuring, and querying personal text: emails, Slack threads, notes, web clips, transcripts, papers, etc.

It does not just store documents. It parses them into structured units, tracks how they evolve over time, and makes them searchable lexically and semantically, with provenance preserved.

Built on SQLite, FTS5, and [sqlite-vec](https://github.com/asg017/sqlite-vec), with optional LLM integration for embeddings, summaries, and information extraction.

**Status:** usable but in early development, API subject to rapid change.

---

## What it does

Turns raw text into a structured, queryable collection:

- Ingests heterogeneous sources (notes, emails, Slack exports, transcripts, web clips, papers)
- Segments them into entries (messages, paragraphs, sections) and chunks (~300-word fragments for indexing, with sentence-boundary and paragraph-boundary snapping)
- Extracts metadata (author, timestamp, hierarchy) when present in the source
- Builds a full-text index (FTS5) and a semantic index (embeddings via sqlite-vec)
- Tracks provenance at every level: document → entry → chunk
- Detects near-duplicates at ingest time using MinHash over 3-word shingles
- Merges growing conversation threads incrementally without duplication
- Generates tiered LLM summaries (brief and detailed, with prompt selection by document length)

Structured segmentation and metadata extraction for non-standard formats (e.g. proprietary email exports, internal Slack threads) require you to supply a custom preprocessor script. `what-was-said` calls this script during ingestion when one is configured for a doctype — the script is responsible for parsing the raw input and emitting the structured representation that `what-was-said` then ingests. See `DEVELOPMENT.md` and the `config.toml` doctype format for details, and `examples/` for working preprocessor scripts.

---

## Key features

**Structured ingestion** — Documents are parsed into entries and chunks, preserving authorship, timestamps, and hierarchy. Markdown parsing is code-fence-aware (comments inside fenced code blocks are not treated as headings).

**Incremental merging** — Conversation-like sources (Slack, email) grow over time. Detects overlap and appends new content without duplication.

**Provenance-first** — Every piece of text is traceable to its source and position within the original document.

**Hybrid search** — FTS5 keyword search for precision; embedding-based semantic search for recall.

**Local-first** — SQLite + sqlite-vec. No external services required.

**LLM integration** — Summaries, embeddings, and structured claim extraction. Works with Ollama locally or any OpenAI-compatible API, including endpoints secured with OAuth2 client credentials.

**Terminal-native interface** — Fast TUI for browsing, reading, tagging, and searching your collection.

---

## Installation

```bash
curl -fsSL https://raw.githubusercontent.com/myersm0/what-was-said/main/install.sh | sh
```

Installs a prebuilt binary to `~/.local/bin/` (Linux x86_64, macOS x86_64, macOS ARM).

### From source

```bash
git clone https://github.com/myersm0/what-was-said.git
cd what-was-said
cargo build --release
cp target/release/what-was-said ~/.local/bin/
```

### Requirements

LLM features are optional but recommended.

**Local (default):**

```bash
# Install Ollama, then pull models
ollama pull qwen3-embedding:8b
ollama pull qwen2.5:32b        # or configure your preferred model in backend.toml
```

**Remote (OpenAI-compatible):**

For standard OpenAI API key auth:
```bash
export OPENAI_API_KEY=...
```

For OAuth2 client credentials (e.g. institutional Azure OpenAI endpoints):
```bash
export OAUTH2_CLIENT_ID=...
export OAUTH2_CLIENT_SECRET=...
```

See [Configuration](#configuration) for `backend.toml` setup.

---

## Quick reference

**Ingest**
```bash
what-was-said ingest ~/inbox/clips/
```

**Browse and search**
```bash
what-was-said browse
what-was-said browse --theme gruvbox
what-was-said about "semantic query"
what-was-said about "keyword query" --method exact
what-was-said in 42
```

**Enrich**
```bash
what-was-said embed              # compute embeddings (chunks + claims)
what-was-said derive             # generate summaries (missing only)
what-was-said derive --force     # regenerate all
what-was-said derive --status    # check progress
what-was-said extract            # extract claims (missing or stale)
what-was-said extract --force    # re-extract all
what-was-said extract --status   # check progress
```

**Serve**
```bash
what-was-said serve              # default port 3030
what-was-said serve --port 8080
```

**JSON output (for scripting)**
```bash
what-was-said about "query" --json
what-was-said about "query" --method exact --json
what-was-said in 42 --json
what-was-said stats --json
what-was-said dump --json
what-was-said derive --status --json
what-was-said extract --status --json
```

Global flags: `--db`, `--config`, `--backend`, `--ollama`, `--model`, `--embed-model`, `--theme`, `--json`.

The `--config` flag specifies the config directory (default: `~/.config/what-was-said/`). The `--backend`, `--ollama`, `--model`, and `--embed-model` flags override their corresponding values from `backend.toml`.

---

## Try it with example data

The `examples/` directory contains sample documents, preprocessor scripts, and a config file. To ingest the demo data:

```bash
pip install -r examples/parsers/requirements.txt

what-was-said --config examples ingest examples/inbox/email/
what-was-said --config examples ingest examples/inbox/slack/
what-was-said --config examples ingest examples/inbox/markdown/

what-was-said browse
```

See `examples/README.md` for details on the example documents and parsers.

---

## Data model

```
Document → Entry → Chunk
```

- **Document** — a source (article, email thread, Slack conversation, voice memo)
- **Entry** — a logical segment within a document (message, paragraph, section)
- **Chunk** — ~300-word fragment used for indexing; boundaries snap to sentence ends and paragraph breaks (500-char max snap distance)

Search operates at the chunk level for precision. Results are grouped and presented at the document level for context.

---

## File format

Clips are plain text files with a `# source:` header:

```
# source: Article Title - Website Name - Browser
Content goes here...
```

The source line is matched against doctype patterns in `config.toml` to determine parsing behavior.

---

## API server

`what-was-said serve` starts a localhost JSON API for programmatic access — the agent-facing interface, with the DB connection held open to avoid per-call process spawn overhead.

| Endpoint | Description |
|---|---|
| `GET /search?q=...&sort=score\|date&author=...&date_from=...&date_to=...` | FTS5 keyword search, results grouped by document |
| `GET /similar?q=...&limit=N&author=...&date_from=...&date_to=...` | Semantic search via embeddings, results grouped by document |
| `GET /get/:id` | Full document with entries and chunks |
| `GET /entries/:doc_id` | Entries for a document |
| `GET /claims/doc/:doc_id` | Claims extracted from a document |
| `GET /claims/similar?q=...&limit=N` | Semantic search over claims |
| `GET /stats` | Database statistics |
| `GET /derive/status` | Derivation progress |

All responses are JSON. Errors return `{"error": "..."}` with appropriate HTTP status codes.

---

## TUI

### Themes

```bash
what-was-said --theme dracula      # default
what-was-said --theme gruvbox
what-was-said --theme nord
what-was-said --theme solarized
what-was-said --theme light
what-was-said --theme ~/mytheme.toml
```

Built-in themes are compiled into the binary. Custom themes are TOML files with hex color values for semantic slots (background, text, borders, highlights, etc.) — see `src/tui/themes/` for the format. Custom themes can also live in `~/.config/what-was-said/themes/` and be referenced by name.

### Keybindings

**Browse**

| Key | Action |
|---|---|
| `↑↓` / `g` / `G` | Navigate |
| `Enter` | Open document |
| `/` | Search |
| `m` / `M` | Mark / clear marks |
| `d` | Brief summary |
| `f` / `F` | Filter by tag / clear |
| `s` / `S` | Cycle sort / toggle direction |
| `q` | Quit |

**Read**

| Key | Action |
|---|---|
| `↑↓` / `g` / `G` | Navigate chunks |
| `PgUp` / `PgDn` | Jump 5 chunks |
| `←→` | Navigate within group (same source title) |
| `d` | Detailed summary |
| `t` | Add/remove tags |
| `y` / `Y` | Yank chunk / full document |
| `/` | Search |
| `b` / `Esc` | Back to browse |

**Search**

| Key | Action |
|---|---|
| `` ` `` | Toggle FTS5 / semantic |
| `Tab` / `Shift-Tab` | Cycle filter fields |
| `↑↓` | Navigate results |
| `Enter` | Open result |
| `Esc` | Back |

**Summary popup**

| Key | Action |
|---|---|
| `↑↓` | Scroll |
| `d` | Toggle brief/detailed |
| `y` | Copy summary |
| `x` | Mark as bad (for regeneration) |
| `Esc` | Close |

---

## Key concepts

**Doctype** — Parsing configuration matched by source title pattern or extension. Defines the parser (whole, markdown, whisper, copilot_email), merge strategy, and optional preprocessor script.

**Merge strategy**
- `none` — each clip creates a new document
- `positional` — clips with the same source title are merged (for growing Slack threads, email chains)

**Derived content** — LLM-generated summaries stored alongside documents. A detailed summary is generated first (prompt tier selected by document length: short/medium/long), then a brief summary is compressed from it. For short documents the brief summary is copied directly without an extra LLM call.

**Near-duplicate detection** — Documents are fingerprinted at ingest time using MinHash over 3-word shingles. New documents are compared against existing documents within a ±180-day window. Jaccard similarity above 0.7 tags the older document as `superseded`; near-misses (0.4–0.7) are logged to stderr.

**Claim extraction** — LLM-extracted atomic propositions from documents. Claims are not facts: they represent what was stated, with provenance and attribution preserved. Each claim records its source document, author (when known), the extraction model, and a prompt hash for staleness detection. An adaptive prompt handles all document types; source-format framings (email attribution, voice memo context) can be configured for structural hints. Claims are embedded into a separate `vec_claims` table for semantic search independent of chunks. Running `what-was-said extract` automatically detects documents needing extraction — including those whose claims were produced by a different model or prompt than what's currently configured.

---

## Configuration

All config lives in `~/.config/what-was-said/` (override with `--config`):

- `config.toml` — doctype definitions and parsing rules
- `backend.toml` — LLM backend selection, model defaults, and authentication
- `tags.toml` — tag hierarchy, default exclusions, tag colors
- `derive.toml` — LLM prompt thresholds and model overrides for summary generation
- `extract.toml` — claim extraction model and prompt overrides

### backend.toml

Selects the LLM backend and provides model defaults. CLI flags (`--backend`, `--model`, `--embed-model`, `--ollama`) override these values when provided.

**Ollama (default, no file needed):**
```toml
backend = "ollama"
ollama_url = "http://localhost:11434"
model = "qwen2.5:32b"
embed_model = "qwen3-embedding:8b"
```

**OpenAI with API key:**
```toml
backend = "openai"
model = "gpt-4.1-mini"
embed_model = "text-embedding-3-small"

[openai]
base_url = "https://api.openai.com/v1"
auth = "api_key"
```
Reads `OPENAI_API_KEY` from the environment.

**OpenAI with OAuth2 (institutional endpoints):**
```toml
backend = "openai"
model = "gpt-4.1-mini"
embed_model = "text-embedding-3-small"

[openai]
base_url = "https://api.openai.wustl.edu/models/v1"
auth = "oauth"
oauth_token_url = "https://login.microsoftonline.com/.../oauth2/v2.0/token"
oauth_scope = "api://.../.default"
```
Reads `OAUTH2_CLIENT_ID` and `OAUTH2_CLIENT_SECRET` from the environment. Tokens are cached and refreshed automatically before expiry.

### Tag colors

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

Available colors: `red`, `green`, `blue`, `cyan`, `magenta`, `yellow`, `white`, `gray`, `light_red`, `light_green`, `light_blue`, `light_cyan`, `light_magenta`, `light_yellow`, or hex values like `"#FF5733"`.

---

## Database

SQLite at `~/.local/share/what-was-said/what-was-said.db`, with WAL mode and foreign key enforcement enabled. All parent-child relationships use `ON DELETE CASCADE`.

**Reset:**
```bash
rm ~/.local/share/what-was-said/what-was-said.db
```

**Rebuild embeddings:**
```sql
DROP TABLE vec_chunks;
DROP TABLE IF EXISTS vec_claims;
```
```bash
what-was-said embed
```

**Re-extract claims:**
```bash
what-was-said extract --force
what-was-said embed
```

---

## Roadmap

**Windowed claim extraction** — Sliding-window extraction over entries for long email/Slack threads (10-15 messages of context, extracting from the middle portion, then advancing). Currently all extraction is whole-document.

**Temporal reasoning** — Structured reasoning over how documents and their claims evolve over time.

**Cross-document linking** — Surfacing connections via semantic similarity across the collection.

**More expressive queries** — Structured filtering beyond keyword and semantic search.

---

## Development

See [DEVELOPMENT.md](DEVELOPMENT.md) for architecture details and internal design.

## License

MIT
