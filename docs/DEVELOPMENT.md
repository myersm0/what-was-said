# Cathedrals Development Guide

Personal knowledge base for clipped documents with full-text and semantic search.

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                          main.rs                                 │
│                    (CLI, orchestration)                          │
└─────────────────────────────────────────────────────────────────┘
         │                    │                    │
         ▼                    ▼                    ▼
┌─────────────┐      ┌─────────────┐      ┌─────────────┐
│   ingest    │      │   storage   │      │     tui     │
│  (parsing)  │      │  (sqlite)   │      │ (ratatui)   │
└─────────────┘      └─────────────┘      └─────────────┘
         │                    │
         ▼                    │
┌─────────────┐               │
│  chunking   │◄──────────────┘
└─────────────┘
```

## Data Model

### Hierarchy

```
Document (1) ──► Entry (n) ──► Chunk (n)
    │               │              │
    │               │              └── chunk_embeddings (1:1)
    │               │
    │               └── author, timestamp, heading
    │
    └── source_title, doctype, merge_strategy, tags
```

### Key Tables

**documents**: Top-level container. One per source (e.g., one Slack channel conversation, one email thread).
- `source_title`: Browser window title or filename
- `doctype`: Matched type (slack, email, markdown, etc.)
- `merge_strategy`: none | positional (for conversations that grow over time)

**entries**: Logical segments within a document (messages, paragraphs, sections).
- `position`: Order within document
- `author`, `timestamp`: For conversations
- `heading_title`, `heading_level`: For structured docs

**chunks**: Text fragments for search indexing. Entries are split into chunks of ~500 chars.
- `chunk_index`: Position within entry
- `body`: The text

**chunks_fts**: FTS5 virtual table for full-text search.

**chunk_embeddings**: Vector embeddings for semantic search. Stored as packed f32 BLOBs.

**document_tags**: Many-to-many relationship for tagging.

## Module Responsibilities

### main.rs
CLI parsing and command dispatch. Orchestrates ingestion flow:
1. Read file, extract `# source:` header
2. Match doctype via config
3. Call appropriate parser/preprocessor
4. Handle merge logic for positional documents
5. Store results

Key functions:
- `ingest_file()`: Main ingestion logic
- `find_overlap()`: Detects overlapping entries for merge
- `extract_merge_key()`: Normalizes source titles for matching

### config.rs
Loads and parses `config.toml` and `tags.toml`. Handles doctype detection.

**Doctype matching** (in order):
1. `source_pattern` regex against source title
2. `extension` match against file extension
3. Content sniffing (markdown headers, copilot email format)

Key types:
- `Doctype`: Parsed config entry
- `DoctypeMatch`: Result of detection, includes parser/preprocessor/merge_strategy

### ingest.rs
Text parsing and segmentation.

**Parsers**:
- `Whole`: Entire file as single entry
- `Markdown`: Split on headings
- `Whisper`: Parse VTT transcripts
- `CopilotEmail`: Parse Copilot-formatted email threads
- `Ollama`: LLM-based segmentation (mostly deprecated)

**External preprocessors**: Python scripts that return JSON. Called via:
```rust
run_preprocessor(script_path, file_path) -> SegmentationResult
```

**OllamaClient**: HTTP client for Ollama API (chat completions and embeddings).

### storage.rs
All SQLite operations. Uses rusqlite directly (no ORM).

Key functions:
- `initialize()`: Creates tables, FTS5, indexes
- `insert_document/entry/chunks`: Write path
- `get_document()`: Read full document with entries and chunks
- `search()` / `search_filtered()`: FTS5 search with grouping
- `find_similar_chunks_filtered()`: Embedding search (brute-force cosine similarity)

**Search result grouping**: Both FTS5 and semantic search return `GroupedSearchResult` — chunks grouped by document, sorted by best score.

### chunking.rs
Splits entry text into chunks for indexing.

Strategy: Split on sentence boundaries, accumulate until ~500 chars, don't break mid-sentence. Falls back to word boundaries for very long sentences.

### tui.rs
Ratatui-based terminal UI.

**Modes**:
- `Browse`: Document list with sorting and filtering
- `Read`: View document content, navigate chunks
- `Search`: FTS5 or semantic search with filters
- `TagEdit`: Add tags to current document
- `TagFilter`: Filter document list by tag

**Search modes** (toggled with F2):
- `Fts5`: Keyword search via SQLite FTS5
- `Semantic`: Vector similarity via embeddings

### types.rs
Shared type definitions: `DocumentId`, `EntryId`, `SegmentedEntry`, `MergeStrategy`, etc.

### minhash.rs
MinHash signatures for near-duplicate detection. Currently used to detect similar entries but grouping logic is minimal.

### merge.rs
Entry merging utilities (combining consecutive same-author messages).

### markdown.rs
Markdown-specific parsing (heading extraction, structure detection).

### whisper.rs
VTT transcript parsing for Whisper output.

## Configuration

### config.toml

```toml
[[doctype]]
name = "slack"
source_pattern = "(Channel|DM).*Slack"
parser = "whole"
merge_strategy = "positional"
preprocessor = "~/.config/cathedrals/parsers/slack_parser.py"
skip = false
merge_consecutive_same_author = true
cleanup_patterns = ["^\\s*:\\w+:\\s*$"]  # Remove emoji-only lines
```

**Fields**:
- `name`: Identifier for this doctype
- `source_pattern`: Regex matched against source title
- `extension`: Alternative match by file extension
- `parser`: whole | markdown | whisper | copilot_email | ollama
- `merge_strategy`: none | positional
- `preprocessor`: Path to external parser script (~ expanded)
- `skip`: If true, files matching this doctype are skipped
- `cleanup_patterns`: Regexes for lines to remove
- `merge_consecutive_same_author`: Combine adjacent same-author entries
- `prompt`: Custom prompt for ollama parser

### tags.toml

```toml
[settings]
default_exclude = ["archive", "noise"]

[aliases]
"hw" = "hardware"
```

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

## Ingestion Flow

```
1. Read file
2. Extract "# source: ..." header line → source_title
3. Match doctype (config.detect_with_content)
4. If skip=true, return early
5. If preprocessor defined, call it; else use built-in parser
6. Apply cleanup_patterns
7. If merge_consecutive_same_author, combine entries
8. If merge_strategy=positional:
   a. Find existing docs with same merge_key
   b. Check each for overlapping entries (≥150 chars consecutive match)
   c. If overlap found, append new entries to existing doc
   d. Else create new document
9. Insert document/entries/chunks
10. Index in FTS5
```

## Search Flow

### FTS5
```
1. Convert query to prefix search ("foo bar" → "foo* bar*")
2. Execute FTS5 MATCH query with snippet()
3. Group results by document_id
4. Deduplicate similar snippets within document
5. Sort by best rank (or date)
```

### Semantic
```
1. Embed query text via Ollama (nomic-embed-text)
2. Load all chunk embeddings from DB
3. Compute cosine similarity against query embedding
4. Filter by author/date if specified
5. Take top N results
6. Group by document_id
7. Convert to GroupedSearchResult format
```

## Embeddings

Stored in `chunk_embeddings` table as packed little-endian f32 BLOBs.

**Generate**: `cathedrals embed [--limit N] [--embed-model MODEL]`

**Model**: nomic-embed-text (768 dimensions) via Ollama

**Search**: Brute-force O(n) cosine similarity. Works well for <50K chunks. Consider sqlite-vec for larger collections.

## Key Design Decisions

1. **Chunk-level search, document-level display**: Search indexes chunks for precision, but results are grouped by document for context.

2. **Positional merge**: Conversation documents (Slack, email) grow over time. Overlap detection allows appending new messages without duplicating.

3. **External preprocessors**: Format-specific parsing is delegated to Python scripts. Easier to iterate on parsing logic without recompiling.

4. **No ORM**: Direct rusqlite for simplicity and control. Schema is simple enough that an ORM adds more complexity than it removes.

5. **Brute-force embeddings**: Simple and dependency-free. The bottleneck is embedding computation, not similarity search.

## Adding a New Parser

1. Define doctype in config.toml with `parser = "whole"` and `preprocessor = "path/to/script.py"`

2. Write Python script that:
   - Reads file path from `sys.argv[1]`
   - Parses content into entries
   - Outputs JSON to stdout

3. Test: `cathedrals ingest path/to/test/file.txt`

## Adding a New CLI Command

1. Add case to `match args.get(1)` in main.rs
2. Implement logic (often calling storage functions)
3. Update `print_usage()`

## Common Maintenance Tasks

**Reset database**: Delete `~/.local/share/cathedrals/cathedrals.db`

**Re-embed everything**: Delete chunk_embeddings rows, run `cathedrals embed`

**Debug ingestion**: Run with file directly, check stderr output

**Profile search**: Add timing around `storage::search()` or `find_similar_chunks()`
