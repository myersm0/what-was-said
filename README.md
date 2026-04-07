# Commonplace

[![CI](https://github.com/myersm0/commonplace/actions/workflows/ci.yml/badge.svg)](https://github.com/myersm0/commonplace/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/myersm0/commonplace)](https://github.com/myersm0/commonplace/releases/latest)

A system for collecting, structuring, and querying personal text: emails, Slack threads, notes, web clips, transcripts, papers, etc.

It does not just store documents. It parses them into structured units, tracks how they evolve over time, and makes them searchable lexically and semantically, with provenance preserved.

Built on SQLite, FTS5, and [sqlite-vec](https://github.com/asg017/sqlite-vec), with optional LLM integration for embeddings, summaries, and information extraction.

The name follows the tradition of the [commonplace book](https://en.wikipedia.org/wiki/Commonplace_book): a personal system for collecting excerpts, observations, and ideas. This project extends that idea into a computational system that can search, summarize, and reason over what you've collected.

**Status:** usable but in early development, API subject to rapid change.

---

## What it does

Commonplace turns raw text into a structured, queryable collection:

- Ingests heterogeneous sources (notes, emails, Slack exports, transcripts, web clips, papers)
- Segments them into entries (messages, paragraphs, sections) and chunks (~300-word fragments for indexing, with sentence-boundary snapping)
- Extracts metadata (author, timestamp, hierarchy) when present in the source
- Builds a full-text index (FTS5) and a semantic index (embeddings via sqlite-vec)
- Tracks provenance at every level: document → entry → chunk
- Detects near-duplicates at ingest time using MinHash over 3-word shingles
- Merges growing conversation threads incrementally without duplication
- Generates tiered LLM summaries (brief and detailed, with prompt selection by document length)

Structured segmentation and metadata extraction for non-standard formats (e.g. proprietary email exports, internal Slack threads) require you to supply a custom preprocessor script. Commonplace calls this script during ingestion when one is configured for a doctype — the script is responsible for parsing the raw input and emitting the structured representation that Commonplace then ingests. See `DEVELOPMENT.md` and the `config.toml` doctype format for details.

---

## Key features

**Structured ingestion** — Documents are parsed into entries and chunks, preserving authorship, timestamps, and hierarchy.

**Incremental merging** — Conversation-like sources (Slack, email) grow over time. Commonplace detects overlap and appends new content without duplication.

**Provenance-first** — Every piece of text is traceable to its source and position within the original document.

**Hybrid search** — FTS5 keyword search for precision; embedding-based semantic search for recall.

**Local-first** — SQLite + sqlite-vec. No external services required.

**LLM integration** — Summaries, embeddings, and (in progress) structured claim extraction. Works with Ollama locally or any OpenAI-compatible API.

**Terminal-native interface** — Fast TUI for browsing, reading, tagging, and searching your collection.

---

## Installation

```bash
curl -fsSL https://raw.githubusercontent.com/myersm0/commonplace/main/install.sh | sh
```

Installs a prebuilt binary to `~/.local/bin/` (Linux x86_64, macOS x86_64, macOS ARM).

### From source

```bash
git clone https://github.com/myersm0/commonplace.git
cd commonplace
cargo build --release
cp target/release/commonplace ~/.local/bin/
```

### Requirements

LLM features are optional but recommended.

**Local (default):**

```bash
# Install Ollama, then pull models
ollama pull qwen3-embedding:8b
ollama pull qwen2.5:32b        # or configure your preferred model in derive.toml
```

**Remote (OpenAI-compatible):**

```bash
export OPENAI_API_KEY=...
commonplace --backend openai ...
```

---

## Quick reference

**Ingest**
```bash
commonplace ingest ~/inbox/clips/
```

**Browse and search**
```bash
commonplace browse
commonplace browse --theme gruvbox
commonplace search "keyword query"
commonplace similar "semantic query"
commonplace get 42
```

**Enrich**
```bash
commonplace embed              # compute embeddings
commonplace derive             # generate summaries (missing only)
commonplace derive --force     # regenerate all
commonplace derive --status    # check progress
```

**Serve**
```bash
commonplace serve              # default port 3030
commonplace serve --port 8080
```

**JSON output (for scripting)**
```bash
commonplace search "query" --json
commonplace similar "query" --json
commonplace get 42 --json
commonplace stats --json
commonplace dump --json
commonplace derive --status --json
```

Global flags: `--db`, `--config`, `--backend`, `--ollama`, `--model`, `--embed-model`, `--theme`, `--json`.

---

## Data model

```
Document → Entry → Chunk
```

- **Document** — a source (article, email thread, Slack conversation, voice memo)
- **Entry** — a logical segment within a document (message, paragraph, section)
- **Chunk** — ~300-word fragment used for indexing; boundaries snap to sentence ends (500-char max snap distance)

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

`commonplace serve` starts a localhost JSON API for programmatic access — the agent-facing interface, with the DB connection held open to avoid per-call process spawn overhead.

| Endpoint | Description |
|---|---|
| `GET /search?q=...&sort=score\|date` | FTS5 keyword search, results grouped by document |
| `GET /similar?q=...&limit=N` | Semantic search via embeddings (default limit 10) |
| `GET /get/:id` | Full document with entries and chunks |
| `GET /entries/:doc_id` | Entries for a document |
| `GET /stats` | Database statistics |
| `GET /derive/status` | Derivation progress |

All responses are JSON. Errors return `{"error": "..."}` with appropriate HTTP status codes.

---

## TUI

### Themes

```bash
commonplace --theme dracula      # default
commonplace --theme gruvbox
commonplace --theme nord
commonplace --theme solarized
commonplace --theme light
commonplace --theme ~/mytheme.toml
```

Built-in themes are compiled into the binary. Custom themes are TOML files with hex color values for semantic slots (background, text, borders, highlights, etc.) — see `src/tui/themes/` for the format. Custom themes can also live in `~/.config/commonplace/themes/` and be referenced by name.

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

**Doctype** — Parsing configuration matched by source title pattern or extension. Defines the parser (whole, markdown, whisper, copilot_email, ollama), merge strategy, and optional preprocessor script.

**Merge strategy**
- `none` — each clip creates a new document
- `positional` — clips with the same source title are merged (for growing Slack threads, email chains)

**Derived content** — LLM-generated summaries stored alongside documents. A detailed summary is generated first (prompt tier selected by document length: short/medium/long), then a brief summary is compressed from it. For short documents the brief summary is copied directly without an extra LLM call.

**Near-duplicate detection** — Documents are fingerprinted at ingest time using MinHash over 3-word shingles. New documents are compared against existing documents within a ±180-day window. Jaccard similarity above 0.7 tags the older document as `superseded`; near-misses (0.4–0.7) are logged to stderr.

---

## Configuration

All config lives in `~/.config/commonplace/`:

- `config.toml` — doctype definitions and parsing rules
- `tags.toml` — tag hierarchy, default exclusions, tag colors
- `derive.toml` — LLM model selection and prompt thresholds

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

SQLite at `~/.local/share/commonplace/commonplace.db`, with WAL mode and foreign key enforcement enabled. All parent-child relationships use `ON DELETE CASCADE`.

**Reset:**
```bash
rm ~/.local/share/commonplace/commonplace.db
```

**Rebuild embeddings:**
```sql
DROP TABLE vec_chunks;
```
```bash
commonplace embed
```

---

## Roadmap

**Claim extraction** *(in design)* — A `claims` table for LLM-extracted atomic propositions from documents. Claims are not facts: they represent what was stated, with provenance and attribution preserved. Each claim records its source document, entry, author (when known), kind (observation, decision, result, recommendation, hypothesis, question, plan, limitation, method), and the model that produced it. Extraction strategy varies by doctype: whole-document for clips and papers, sliding-window over entries for long threads. Claims will be embedded for semantic search alongside chunks, and exposed via the `serve` API.

**Temporal reasoning** — Structured reasoning over how documents and their claims evolve over time.

**Cross-document linking** — Surfacing connections via semantic similarity across the collection.

**LLM backend abstraction** — A clean trait interface for swapping backends (Ollama, OpenAI, others).

**More expressive queries** — Structured filtering beyond keyword and semantic search.

---

## Development

See [DEVELOPMENT.md](DEVELOPMENT.md) for architecture details and internal design.

## License

MIT
