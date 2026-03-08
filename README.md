# Cathedrals

My personal knowledge base for notes and documents. Stores web clips, notes, whisper-transcribed voice memos, emails, papers, etc, in SQLite with full-text and semantic search.

## Installation
After ensuring the prerequisites (see section below) are met:
```bash
curl -fsSL https://raw.githubusercontent.com/myersm0/cathedrals/main/install.sh | sh
```

This downloads a prebuilt binary for your platform (Linux x86_64, macOS x86_64, macOS ARM) and installs it to ~/.local/bin/.

From source:
```bash
git clone https://github.com/myersm0/cathedrals.git
cd cathedrals
cargo build --release
cp target/release/cathedrals ~/.local/bin/
```

## Prerequisites
- Install [ollama](https://ollama.com/) (on a Mac you can do `brew install ollama`)
- ollama running locally with `ollama serve`
- Pull an embedding model: `ollama pull nomic-embed-text`
- Pull a summarization model: `ollama pull qwen2.5:32b` (or configure your preferred model in derive.toml)

## Quick Reference

```bash
# Ingest new clips from inbox
cathedrals ingest ~/inbox/clips/

# Browse collection
cathedrals browse

# Search from CLI
cathedrals search "keyword query"
cathedrals similar "semantic query"

# Generate LLM summaries
cathedrals derive              # missing only
cathedrals derive --force      # regenerate all
cathedrals derive --status     # check progress

# Compute embeddings for semantic search
cathedrals embed
```

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
- `s` — cycle sort column
- `q` — quit

**Read mode**
- `↑↓` / `g` / `G` — navigate chunks
- `←→` — navigate within group (same source title)
- `d` — view detailed summary
- `t` — add tag
- `y` — yank current chunk
- `Y` — yank full document
- `b` / `Esc` — back to browse

**Search mode**
- `F2` — toggle FTS5 / Semantic
- `Tab` — cycle filter fields (query, author, date range)
- `Enter` — open result
- `Esc` — back

**Summary popup**
- `d` — toggle brief/detailed
- `y` — copy summary
- `x` — mark as bad (for regeneration)
- `Esc` — close

## Key Concepts

**Doctype**: Parsing configuration matched by source title pattern or extension. Defines parser (whole, markdown, whisper, etc.), merge strategy, and optional preprocessor script.

**Merge strategy**: 
- `none` — each clip creates a new document
- `positional` — clips with same source title are merged (for growing Slack threads, email chains)

**Entries**: Segments within a document (messages, paragraphs, sections).

**Chunks**: ~500 char fragments of entries, indexed for search.

**Derived content**: LLM-generated summaries (brief + detailed) stored alongside documents. Prompt tier (short/medium/long) is selected based on document length.

## Config Files

All in `~/.config/cathedrals/`:

- `config.toml` — doctype definitions
- `tags.toml` — tag aliases, default exclusions
- `derive.toml` — LLM model selection, thresholds

See `DEVELOPMENT.md` for architecture details.

## Database

SQLite at `~/.local/share/cathedrals/cathedrals.db`

To reset: delete the db file. To re-embed: `DELETE FROM chunk_embeddings;` then `cathedrals embed`.
