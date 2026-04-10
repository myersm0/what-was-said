# Examples

Test data and parser scripts for what-was-said ingestion.

## Quick start

```bash
# from the repo root
pip install -r examples/parsers/requirements.txt

what-was-said --config examples ingest examples/inbox/email/
what-was-said --config examples ingest examples/inbox/slack/
what-was-said --config examples ingest examples/inbox/markdown/

what-was-said browse
```

The `--config examples` flag tells what-was-said to use `examples/` as the config directory, picking up `examples/config.toml` with the parser paths.

## Structure

```
examples/
├── config.toml     # doctype config wired to the example parsers
├── inbox/          # sample clips, organized by doctype
│   ├── email/      # Outlook email chains (.txt)
│   ├── markdown/   # markdown documents (.md)
│   └── slack/      # Slack conversations (.txt)
└── parsers/        # external preprocessor scripts
    ├── email_parser.py
    ├── junk.txt
    ├── requirements.txt
    └── slack_parser.py
```

## Parsing

Each doctype is parsed differently. Markdown parsing is handled internally by what-was-said. Email and Slack require external preprocessor scripts that what-was-said calls during ingestion; each script takes a file path and emits structured JSON to stdout.

### Email

Parsed by `parsers/email_parser.py`. Outlook email chains are split into individual messages, oldest first. Each entry has an author, ISO 8601 timestamp (null for the topmost/newest message in the chain, which lacks a machine-readable date), and body text.

The parser handles several Outlook clipboard format variants: quoted-name and unquoted-name `From:` lines, `From:` lines with or without email addresses, email addresses glued to names without whitespace, mailing list wrappers (`'Name' via List`), zero-width-space-delimited recipient blocks, and Outlook's timestamp quirks (narrow no-break spaces, 24-hour-plus-AM/PM).

Signature and boilerplate stripping is driven by `junk.txt` (a human-curated list of lines to remove, located alongside the script).

Usage:
```bash
python parsers/email_parser.py inbox/email/20260306_12-01-59.txt
python parsers/email_parser.py --generate-candidates inbox/email/
python parsers/email_parser.py --check inbox/email/
```

### Markdown

Parsed internally by what-was-said. Documents are segmented into entries by heading structure. Code-fence-aware: `#` lines inside fenced code blocks are not treated as headings. No external preprocessor is needed.

### Slack

Parsed by `parsers/slack_parser.py`. Slack conversations are split into individual messages. Consecutive messages by the same author are merged into a single entry. Each entry has an author and body text; timestamps are discarded (too inconsistent across formats to be useful).

The parser handles three distinct clipboard layout variants: bracket format with newline-delimited entries (`Author  [10:01 AM]`), bracket format with inline/no-newline entries (messages jammed together, separated only by `author  [time]` patterns, often using non-breaking spaces), and indented format (author on one line, timestamp indented on the next). Format is auto-detected per file.

Timestamp formats handled: `[10:01 AM]`, `Today at 6:30 AM`, `Yesterday at 6:07 PM`, `Sunday at 4:12 PM`, `Nov 8th at 12:54 PM`, `3:24 PM`, bare continuations like `3:26`, and relative timestamps like `40 minutes ago`. New-author detection requires a "full" timestamp (AM/PM, date/day prefix, or relative); bare times are always treated as continuations of the current author, preventing false splits on timestamps appearing in message body text (e.g. stack traces, log output).

Noise stripping covers: reply count indicators (`N replies`), standalone emoji lines (`:+1:`, `:white_check_mark:`), reaction counts, `image.png` placeholders, and inline bracketed timestamps within a single author's message (replaced with linebreaks). Author name cleanup strips emoji flair (`:corn:`, `:talon:`) and reply counts jammed onto names.

Usage:
```bash
python parsers/slack_parser.py inbox/slack/20260205_14-29-01.txt
```
