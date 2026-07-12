# what-was-said

[![CI](https://github.com/myersm0/what-was-said/actions/workflows/ci.yml/badge.svg)](https://github.com/myersm0/what-was-said/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/myersm0/what-was-said)](https://github.com/myersm0/what-was-said/releases/latest)

A system for collecting, structuring, and querying personal text: emails, Slack threads, notes, web clips, transcripts, papers, etc.

It does not just store documents. It parses them into structured units, tracks how they evolve over time, and makes them searchable lexically and semantically, with provenance preserved.

Built on SQLite, FTS5, and [sqlite-vec](https://github.com/asg017/sqlite-vec), with optional LLM integration for embeddings, summaries, claim extraction, and near-duplicate change summaries.

**Status:** usable but in early development, API subject to rapid change.

---

## What it does

Turns raw text into a structured, queryable collection:

- Ingests heterogeneous sources (notes, emails, Slack exports, transcripts, web clips, papers)
- Segments them into entries (messages, paragraphs, sections) and chunks (~300-word fragments for indexing, with sentence-boundary and paragraph-boundary snapping)
- Extracts metadata (author, timestamp, hierarchy) when present in the source
- Builds a full-text index (FTS5) and a semantic index (embeddings via sqlite-vec)
- Tracks provenance at every level: document → entry → chunk
- Detects near-duplicates at ingest time using MinHash over 3-word shingles plus a longest-shared-block signal, resolving them automatically or by prompting you, and records every decision
- Merges growing conversation threads incrementally without duplication
- Generates tiered LLM summaries (brief and detailed, with prompt selection by document length)
- Summarizes what changed between near-duplicate versions with an LLM, on demand at ingest or in a batch pass

Structured segmentation and metadata extraction for non-standard formats (e.g. proprietary email exports, internal Slack threads) require you to supply a custom preprocessor script. `what-was-said` calls this script during ingestion when one is configured for a doctype — the script is responsible for parsing the raw input and emitting the structured representation that `what-was-said` then ingests. See `DEVELOPMENT.md` and the `config.toml` doctype format for details, and `examples/` for working preprocessor scripts.

---

## Key features

**Structured ingestion** — Documents are parsed into entries and chunks, preserving authorship, timestamps, and hierarchy. Markdown parsing is code-fence-aware (comments inside fenced code blocks are not treated as headings).

**Incremental merging** — Conversation-like sources (Slack, email) grow over time. Detects overlap and appends new content without duplication.

**Near-duplicate handling** — Revisions of an already-clipped document are detected by MinHash/Jaccard and a longest-shared-block heuristic. Matching versions form a *version family*, and the latest by clip date is the current one; older members are tagged `superseded` (both are always kept). `superseded` is an ordinary tag: browse and search — TUI, CLI, and the serve API alike — hide it through the `[defaults] exclude` list in tags.toml (the compiled-in default applies when no tags.toml exists), and `--include-all` (or `include_hidden=true` on the API) brings old versions back, annotated with a pointer to the current version. Borderline matches are resolved by an interactive prompt. Every decision is recorded as a queryable relation, and an LLM can summarize what changed between versions.

**Provenance-first** — Every piece of text is traceable to its source and position within the original document.

**Hybrid search** — FTS5 keyword search for precision; embedding-based semantic search for recall.

**Local-first** — SQLite + sqlite-vec. No external services required.

**LLM integration** — Summaries, embeddings, structured claim extraction, and near-duplicate change summaries. Works with Ollama locally or any OpenAI-compatible API, including endpoints secured with OAuth2 client credentials.

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
ollama pull qwen2.5:32b        # or configure your preferred model in llms.toml
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

See [Configuration](#configuration) for `llms.toml` setup.

---

## Quick reference

**Ingest**
```bash
what-was-said ingest ~/inbox/clips/          # a directory, or a single file
what-was-said ingest ~/inbox/clips/note.md   # prompts on gray-zone near-duplicates
```

Ingest accepts a file or a directory. Gray-zone near-duplicates (similar, but not clearly a revision) prompt you to supersede, keep both, view a text diff, view an LLM summary of the change, or quit. Ingest is always interactive: if a gray-zone match arises with no controlling terminal (a piped or scripted run), ingest aborts on that file rather than guessing, naming the document pair.

**Browse and search**
```bash
what-was-said browse
what-was-said browse --theme gruvbox
what-was-said about "semantic query"
what-was-said about "keyword query" --method exact
what-was-said about "query" --project game   # restrict to one project
what-was-said about "query" --include-all    # include superseded/junk/etc.
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
what-was-said diff               # summarize near-duplicate changes (missing or stale)
what-was-said diff --force        # re-summarize all relations
what-was-said relations repair    # recompute superseded tags across version families
what-was-said relations repair --family 501   # repair only one family
what-was-said relations scan      # retroactively detect near-duplicates across the collection
```

**Sync (curated project docs)**
```bash
what-was-said sync                       # sync all registered projects
what-was-said sync --project game        # sync one project
```

Sync ingests curated project documents from their on-disk source of truth (see [Project sync](#project-sync)). Project docs are not embedded by sync; run `embed` (and `derive`/`extract`) afterward, as with captured clips.

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

The `--config` flag specifies the config directory (default: `~/.config/what-was-said/`). The `--backend`, `--ollama`, `--model`, and `--embed-model` flags override their corresponding values from `llms.toml`.

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
| `GET /search?q=...&sort=score\|date&author=...&date_from=...&date_to=...&project=...&include_hidden=true` | FTS5 keyword search, results grouped by document |
| `GET /similar?q=...&limit=N&author=...&date_from=...&date_to=...&project=...&include_hidden=true` | Semantic search via embeddings, results grouped by document |
| `GET /get/:id` | Full document with entries and chunks |
| `GET /entries/:doc_id` | Entries for a document |
| `GET /claims/doc/:doc_id` | Claims extracted from a document |
| `GET /claims/similar?q=...&limit=N&include_hidden=true` | Semantic search over claims |
| `GET /stats` | Database statistics |
| `GET /derive/status` | Derivation progress |

All responses are JSON. Errors return `{"error": "..."}` with appropriate HTTP status codes.

Search endpoints hide documents tagged with a default-excluded tag (`superseded`, `junk`, etc. per `tags.toml`) unless `include_hidden=true`. Direct fetches always return the requested document. Whenever a superseded document appears in a response — by opt-in, direct fetch, or a claim sourced from it — it carries `superseded: true` and `current_document_id` pointing at the family's current version, so a consumer can always route to the live document. Non-superseded documents carry `superseded: false`.

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

**Near-duplicate detection** — Documents are fingerprinted at ingest time using MinHash over 3-word shingles, and compared against existing documents within a ±180-day window. Only a Jaccard similarity ≥ 0.7 merges unattended: the new document and *every* such candidate are linked into one version family, and the family's `superseded` tags are recomputed so that only the latest member by `(clip_date, id)` is current. Because "current" is a projection of clip date over the whole family rather than a side effect of insertion order, out-of-order ingestion still resolves correctly. Everything below that band can only ever route to the interactive prompt (supersede, keep both, view a text diff, view an LLM summary of the change, or quit), which offers the chronological predecessor as the comparison; with no controlling terminal, ingest aborts on that file rather than guessing. Four routes reach the prompt: Jaccard between 0.4 and 0.7 (the gray zone); a matching source title with Jaccard ≥ 0.1 (a re-clip of the same page or conversation, revised enough to fool the similarity bands); a sub-0.4 candidate whose containment (overlap coefficient) confirms that one document is largely contained in the other, catching revisions that grew or shrank substantially; and a sub-0.4 candidate that fails containment but shares a contiguous block of ≥ 300 words, catching heavy revisions that keep a large section intact. Every decision is recorded in `document_relations` (see below).

**Document relations** — Each near-duplicate decision is persisted as a row in `document_relations`, recording the two documents, the similarity, the shared block size, and the resolution (`superseded` or `kept_both`). A row records only that two documents belong to one version family; it carries no direction and no current flag. Which member is current is *derived* by sorting the family by `(clip_date, id)` — so the `superseded` tag is a recomputed cache, rewritten wholesale for an affected family on every insert, never pointer-surgered. `what-was-said relations repair [--family DOC_ID]` re-runs that recomputation over every family (or one), which fixes any family left mis-tagged by older versions of the ingester. The relations table is also a queryable audit trail and the input to diff summaries: `what-was-said diff` generates an LLM description of what changed between each pair (from only the non-overlapping regions, not the full documents) and stores it on the relation, with the model and prompt hash for staleness detection — so changing the model or prompt re-summarizes automatically. The same summary can be generated on demand during an interactive ingest prompt via the `[v]iew LLM summary` option, which also stores it on the relation.

**Claim extraction** — LLM-extracted atomic propositions from documents. Claims are not facts: they represent what was stated, with provenance and attribution preserved. Each claim records its source document, author (when known), the extraction model, and a prompt hash for staleness detection. An adaptive prompt handles all document types; source-format framings (email attribution, voice memo context) can be configured for structural hints. Claims are embedded into a separate `vec_claims` table for semantic search independent of chunks. Running `what-was-said extract` automatically detects documents needing extraction — including those whose claims were produced by a different model or prompt than what's currently configured.

---

## Project sync

The system handles two kinds of text. **Captured** documents are evidence — clips, emails, Slack threads, transcripts — preserved with provenance and never edited in place. **Curated** project documents are state you actively maintain — a README, a design doc, working notes — where the system should track the *current* version rather than accumulate snapshots. Project sync keeps curated docs in step with their on-disk source of truth.

A curated doc's identity is its position, `(project, relative_path)` — not its content. Editing a file updates the same logical document in place; near-duplicate detection does **not** run for project docs.

**Registering projects.** Add a `projects.toml` to the config directory:

```toml
[[project]]
name = "game"
manifest = "~/projects/game/documents.toml"
# root = "~/projects/game"   # optional; defaults to the manifest's directory
```

**Manifests.** Each project has a `documents.toml` mapping file globs to a status and an optional role. Rules are matched top-to-bottom, first match wins; files matching no rule are skipped (inclusion is explicit):

```toml
[[docs]]
glob = "README.md"
status = "canonical"
role = "overview"

[[docs]]
glob = "ideas/*.md"
status = "provisional"
role = "idea"

[[docs]]
glob = "logs/*.md"
status = "archived"
role = "log"
```

Manifest statuses are `canonical`, `provisional`, and `archived`. A fourth status, `missing`, is set only by sync (never declared in a manifest) when a previously-synced file disappears from disk; such docs are tombstoned rather than deleted, since the file may reappear and the old content stays useful for search.

**Syncing.** `what-was-said sync [--project NAME]` walks each project's root, matches files against the manifest, and upserts by `(project, relative_path)`: new files are parsed (markdown) and inserted; changed files (detected by content hash) have their entries, chunks, claims, embeddings, and summaries replaced under the same document id; unchanged files are skipped. Vanished files are marked `missing`. It prints a `N new, N updated, N unchanged, N missing` summary. Project docs are not embedded by sync — run `embed` (and `derive`/`extract` for summaries and claims) afterward.

**Searching projects.** `what-was-said about "..." --project NAME` restricts results to one project. For semantic search this does an exact nearest-neighbor scan over just that project's chunks, so recall is unaffected by the size of the captured corpus. Across all searches, document status nudges ranking: `canonical` results outrank `provisional`, which outrank `archived` and `missing`. Captured docs are unaffected (neutral weight). In the TUI, the project name appears in the browse Type column and the read view header; the search filter row includes a Project field.

---

## Configuration

All config lives in `~/.config/what-was-said/` (override with `--config`):

- `config.toml` — doctype definitions and parsing rules
- `llms.toml` — all LLM configuration: backend selection, authentication, model defaults, and per-task (derive/extract/diff) overrides
- `tags.toml` — tag hierarchy, default exclusions, tag colors. Read from the config directory; the legacy data-directory location is still honored, and the compiled-in default (which excludes `junk`, `archived`, `superseded`) applies when neither file exists
- `projects.toml` — registry of curated projects to sync (see [Project sync](#project-sync)); absent by default

`llms.toml` replaces the older `backend.toml`, `derive.toml`, and `extract.toml`. If `llms.toml` is absent, those legacy files are still read as a fallback, so existing setups keep working until you migrate.

### llms.toml

Selects the LLM backend, provides authentication and model defaults, and configures the per-task models and thresholds. CLI flags (`--backend`, `--model`, `--embed-model`, `--ollama`) override the top-level values when provided. Each per-task model falls back to the top-level `model` when its section (or its `model` key) is omitted, so you only set a section when a task needs a different model from the default.

**Ollama (default, no file needed):**
```toml
backend = "ollama"
ollama_url = "http://localhost:11434"
model = "qwen2.5:32b"          # default generation model for every task
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

**Per-task sections (all optional):**
```toml
[derive]
detailed_model = "qwen2.5:32b"   # falls back to top-level model if omitted
brief_model = "qwen2.5:32b"
short_threshold = 1200
medium_threshold = 3500
[derive.prompts]
short = "~/.config/what-was-said/prompts/detailed_short.txt"

[extract]
model = "gemma3:27b"             # claim extraction; falls back to top-level model
[extract.framings]
email = "~/.config/what-was-said/prompts/extract_framing_conversation.txt"

[diff]
model = "qwen2.5:32b"            # near-duplicate change summaries; falls back to top-level model
```

### Tag colors

```toml
[defaults]
exclude = ["junk", "archived", "superseded"]

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
