# Cathedrals

My personal knowledge base for notes and documents. Stores web clips, notes, whisper-transcribed voice memos, emails, papers, etc, in SQLite with full-text and semantic search.

## Installation

```bash
curl -fsSL https://raw.githubusercontent.com/myersm0/cathedrals/main/install.sh | sh
```

This downloads a prebuilt binary for your platform (Linux x86_64, macOS x86_64, macOS ARM) and installs it to `~/.local/bin/`.

### From source

```bash
git clone https://github.com/myersm0/cathedrals.git
cd cathedrals
cargo build --release
cp target/release/cathedrals ~/.local/bin/
```

### Requirements

- [ollama](https://ollama.com) running locally for embeddings and LLM summaries (default backend)
- Pull an embedding model: `ollama pull qwen3-embedding:8b`
- Pull a summarization model: `ollama pull qwen2.5:32b` (or configure your preferred model in `derive.toml`)

Alternatively, use an OpenAI-compatible API by setting `OPENAI_API_KEY` (and optionally `OPENAI_API_BASE`) and passing `--backend openai`.

## Quick Reference

```bash
# Ingest new clips from inbox
cathedrals ingest ~/inbox/clips/

# Browse collection
cathedrals browse

# Search from CLI
cathedrals search "keyword query"
cathedrals similar "semantic query"

# Get a specific document
cathedrals get 42

# Generate LLM summaries
cathedrals derive              # missing only
cathedrals derive --force      # regenerate all
cathedrals derive --status     # check progress

# Compute embeddings for semantic search
cathedrals embed

# Start JSON API server
cathedrals serve               # default port 3030
cathedrals serve --port 8080

# JSON output for scripting
cathedrals search "query" --json
cathedrals similar "query" --json
cathedrals get 42 --json
cathedrals stats --json
cathedrals dump --json
cathedrals derive --status --json

# Use OpenAI-compatible backend
cathedrals --backend openai --embed-model text-embedding-3-small similar "query"
```

## API Server

`cathedrals serve` starts a localhost JSON API for programmatic access. This is the agent-facing interface — holds the DB connection open and avoids per-call process spawn overhead.

Endpoints:

- `GET /search?q=...&sort=score|date` — FTS5 keyword search, results grouped by document
- `GET /similar?q=...&limit=N` — semantic search via embeddings (default limit 10)
- `GET /get/:id` — full document with entries and chunks
- `GET /entries/:doc_id` — entries for a document
- `GET /stats` — database statistics
- `GET /derive/status` — derivation progress

All responses are JSON. Errors return `{"error": "..."}` with appropriate HTTP status codes.

## File Format

Clips are text files with a `# source:` header line:

```
# source: Article Title - Website Name - Browser
Clipped content goes here...
```

The source line is matched against doctype patterns in `config.toml` to determine parsing strategy.

## TUI Keybindings

**Browse mode**
- `↑↓` / `g` / `G` — navigate
- `Enter` — open document
- `/` — search
- `m` — mark document (for multi-doc navigation)
- `M` — clear marks
- `d` — view brief summary
- `f` — filter by tag
- `F` — clear tag filter
- `s` — cycle sort column
- `S` — toggle sort direction
- `q` — quit

**Read mode**
- `↑↓` / `g` / `G` — navigate chunks
- `PgUp` / `PgDn` — jump 5 chunks
- `←→` — navigate within group (same source title)
- `d` — view detailed summary
- `t` — add/remove tags
- `y` — yank current chunk
- `Y` — yank full document
- `/` — search
- `b` / `Esc` — back to browse

**Search mode**
- `F2` — toggle FTS5 / Semantic
- `Tab` / `Shift-Tab` — cycle filter fields (query, author, date range)
- `↑↓` — navigate results
- `Enter` — open result
- `Esc` — back

**Summary popup**
- `↑↓` — scroll
- `d` — toggle brief/detailed
- `y` — copy summary
- `x` — mark as bad (for regeneration)
- `Esc` — close

## Key Concepts

**Doctype**: Parsing configuration matched by source title pattern or extension. Defines parser (whole, markdown, whisper, copilot_email, ollama), merge strategy, and optional preprocessor script.

**Merge strategy**: 
- `none` — each clip creates a new document
- `positional` — clips with same source title are merged (for growing Slack threads, email chains)

**Entries**: Segments within a document (messages, paragraphs, sections).

**Chunks**: ~500 char fragments of entries, indexed for search.

**Derived content**: LLM-generated summaries stored alongside documents. A detailed summary is generated first (prompt tier selected by document length: short/medium/long), then a brief summary is compressed from it. For short documents, the brief summary is copied directly from the detailed summary without an extra LLM call.

## Config Files

All in `~/.config/cathedrals/`:

- `config.toml` — doctype definitions
- `tags.toml` — tag hierarchy, default exclusions, tag colors
- `derive.toml` — LLM model selection, prompt thresholds

### Tag Colors

Tags can be assigned colors in `tags.toml`, displayed as colored markers in the browse view:

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

Available colors: red, green, blue, cyan, magenta, yellow, white, gray, light_red, light_green, light_blue, light_cyan, light_magenta, light_yellow.

See `DEVELOPMENT.md` for architecture details.

## Database

SQLite at `~/.local/share/cathedrals/cathedrals.db` with WAL mode enabled for concurrent read access.

To reset: delete the db file. To re-embed: drop the vec table (`DROP TABLE vec_chunks;`) then `cathedrals embed`.
