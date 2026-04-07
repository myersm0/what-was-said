import re
import sys
import json
from dataclasses import dataclass
from pathlib import Path

sys.stdout.reconfigure(encoding='utf-8')


@dataclass
class SlackEntry:
	author: str
	body: str


TIME_PATTERN = r'\d{1,2}:\d{2}(?:\s*(?:AM|PM))?'
DATE_PREFIX = (
	r'(?:(?:Today|Yesterday'
	r'|(?:Mon|Tues|Wednes|Thurs|Fri|Satur|Sun)day'
	r'|(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)'
	r'\s+\d+(?:st|nd|rd|th)?)\s+at\s+)?'
)
FULL_TIME = DATE_PREFIX + TIME_PATTERN

BRACKET_BOUNDARY = re.compile(
	rf'(?P<author>\S.*?)\s{{2,}}\[(?P<time>{TIME_PATTERN})\]'
)
BARE_BRACKET_CONTINUATION = re.compile(
	rf'^\[(?P<time>{TIME_PATTERN})\]'
)
INDENTED_TIMESTAMP = re.compile(
	rf'^\s*(?::[\w_]+:\s*)*\s*(?P<time>{FULL_TIME})\s*$'
)
INDENTED_FULL_TIMESTAMP = re.compile(
	rf'^\s*(?::[\w+\-]+:\s*)*\s*(?P<time>'
	rf'(?:(?:Today|Yesterday'
	rf'|(?:Mon|Tues|Wednes|Thurs|Fri|Satur|Sun)day'
	rf'|(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)'
	rf'\s+\d+(?:st|nd|rd|th)?)\s+at\s+)?'
	rf'\d{{1,2}}:\d{{2}}\s*(?:AM|PM)'
	rf'|\d+\s+(?:minute|hour)s?\s+ago'
	rf')\s*$'
)
BARE_CONTINUATION_TIME = re.compile(
	rf'^(?P<time>{TIME_PATTERN})\s*$'
)

NOISE_LINE_PATTERNS = [
	re.compile(r'^\d+\s+repl(?:y|ies)\s*$'),
	re.compile(r'^:[\w+\-]+:\s*$'),
	re.compile(r'^\d+\s*$'),
	re.compile(r'^image\.png\s*$'),
]

LINK_UNFURL_SIGNALS = re.compile(
	r'^(?:GitLab|GitHub|Stack Overflow|Wikipedia)(?:GitLab|GitHub)?\s*$'
)

AUTHOR_NOISE_SUFFIX = re.compile(
	r'(?:\d+\s+repl(?:y|ies))\s*$'
)

AUTHOR_NOISE_PREFIX = re.compile(
	r'^(?:\d+[\s\xa0]*repl(?:y|ies))[\s\xa0]*([A-Z])'
)

FLAIR_STRIP = re.compile(r'(?::[\w+\-]+:\s*)+')

REPLY_NOISE_BEFORE_AUTHOR = re.compile(
	rf'\d+[\s\xa0]*repl(?:y|ies)(?=\S.*?[\s\xa0]{{2,}}\[{TIME_PATTERN}\])'
)


def is_space(char):
	return char in (' ', '\xa0', '\t')


def is_boundary(char):
	return char in (' ', '\xa0', '\t', '\n', ']', ')', '.')


def is_noise_line(line):
	stripped = line.strip()
	if not stripped:
		return False
	for pattern in NOISE_LINE_PATTERNS:
		if pattern.match(stripped):
			return True
	if LINK_UNFURL_SIGNALS.match(stripped):
		return True
	return False


def strip_source_line(text):
	lines = text.split('\n')
	if lines and lines[0].startswith('# source:'):
		return '\n'.join(lines[1:]).strip()
	return text.strip()


def detect_format(text):
	if re.search(rf'\S.*?\s{{2,}}\[{TIME_PATTERN}\]', text):
		return "bracket"
	for line in text.split('\n'):
		if INDENTED_TIMESTAMP.match(line):
			return "indented"
	return "unknown"


def clean_author(name):
	name = AUTHOR_NOISE_SUFFIX.sub('', name).strip()
	name = AUTHOR_NOISE_PREFIX.sub(r'\1', name)
	name = FLAIR_STRIP.sub('', name).strip()
	return name


INLINE_BRACKET_TIME = re.compile(
	rf'\[{TIME_PATTERN}\]'
)


def clean_body_lines(lines):
	cleaned = []
	for line in lines:
		if is_noise_line(line):
			continue
		cleaned.append(line)
	while cleaned and not cleaned[0].strip():
		cleaned.pop(0)
	while cleaned and not cleaned[-1].strip():
		cleaned.pop()
	text = '\n'.join(cleaned)
	text = INLINE_BRACKET_TIME.sub('\n', text)
	return text.strip()


def extract_trailing_author(text):
	text = text.rstrip(' \xa0\t')
	if not text:
		return ""
	words = []
	pos = len(text)
	max_words = 4
	while pos > 0 and len(words) < max_words:
		while pos > 0 and is_space(text[pos - 1]):
			pos -= 1
		if pos == 0:
			break
		word_end = pos
		while pos > 0 and not is_boundary(text[pos - 1]):
			pos -= 1
		word = text[pos:word_end]
		if not word:
			break
		if re.match(r'^[@\w][\w\'-]*$', word):
			words.append(word)
		else:
			break
		if pos > 0 and not is_space(text[pos - 1]):
			break
	words.reverse()
	return ' '.join(words)


# ── Bracket format ──

def find_bracket_boundaries(text):
	delimiter = re.compile(rf'\s{{2,}}\[({TIME_PATTERN})\]')
	matches = list(delimiter.finditer(text))
	if not matches:
		return []

	boundaries = []
	for match in matches:
		time_str = match.group(1)
		before_spaces = text[:match.start()]
		author = extract_trailing_author(before_spaces)
		author_start = match.start() - len(author) if author else match.start()
		author = clean_author(author)
		body_start = match.end()
		boundaries.append({
			'author': author,
			'timestamp': time_str,
			'author_start': author_start,
			'body_start': body_start,
		})

	entries = []
	for i, b in enumerate(boundaries):
		if i + 1 < len(boundaries):
			body_end = boundaries[i + 1]['author_start']
		else:
			body_end = len(text)
		body = text[b['body_start']:body_end].strip()
		entries.append(SlackEntry(b['author'], body))
	return entries


def parse_bracket_format(text):
	text = REPLY_NOISE_BEFORE_AUTHOR.sub('', text)
	lines = text.split('\n')
	entries = []
	current_author = None
	current_body_lines = []

	def flush():
		nonlocal current_body_lines
		if current_author:
			body = clean_body_lines(current_body_lines)
			if body:
				entries.append(SlackEntry(current_author, body))
		current_body_lines = []

	for line in lines:
		bracket_match = BRACKET_BOUNDARY.search(line)
		if bracket_match:
			all_matches = list(BRACKET_BOUNDARY.finditer(line))
			if len(all_matches) > 1:
				flush()
				inline_entries = find_bracket_boundaries(line)
				for ie in inline_entries[:-1]:
					entries.append(ie)
				if inline_entries:
					last = inline_entries[-1]
					current_author = last.author
					current_body_lines = [last.body] if last.body else []
				continue

			flush()
			current_author = clean_author(bracket_match.group('author'))
			remainder = line[bracket_match.end():]
			current_body_lines = [remainder] if remainder.strip() else []
			continue

		cont_match = BARE_BRACKET_CONTINUATION.match(line)
		if cont_match and current_author:
			flush()
			remainder = line[cont_match.end():]
			current_body_lines = [remainder] if remainder.strip() else []
			continue

		current_body_lines.append(line)

	flush()
	return entries


# ── Indented format ──

def parse_indented_format(text):
	lines = text.split('\n')
	entries = []
	current_author = None
	current_body_lines = []

	def flush():
		nonlocal current_body_lines
		if current_author:
			body = clean_body_lines(current_body_lines)
			if body:
				entries.append(SlackEntry(current_author, body))
		current_body_lines = []

	i = 0
	while i < len(lines):
		line = lines[i]

		if i + 1 < len(lines):
			ts_match = INDENTED_FULL_TIMESTAMP.match(lines[i + 1])
			if ts_match:
				stripped = line.strip()
				candidate = FLAIR_STRIP.sub('', stripped).strip()
				if candidate and not is_noise_line(line):
					is_first_entry = current_author is None
					has_body = len(current_body_lines) > 0 and any(
						l.strip() for l in current_body_lines
					)
					if is_first_entry or has_body:
						flush()
						current_author = candidate
						current_body_lines = []
						i += 2
						continue

		stripped = line.strip()
		cont_match = BARE_CONTINUATION_TIME.match(stripped)
		if cont_match and current_author and stripped == cont_match.group(0):
			flush()
			current_body_lines = []
			i += 1
			continue

		if not is_noise_line(line):
			current_body_lines.append(line)
		i += 1

	flush()
	return entries


# ── Merge consecutive same-author entries ──

def merge_consecutive(entries):
	if not entries:
		return entries
	merged = [entries[0]]
	for entry in entries[1:]:
		if entry.author == merged[-1].author:
			merged[-1].body += '\n' + entry.body
		else:
			merged.append(entry)
	return merged


# ── Dispatch ──

def parse_slack(text):
	text = strip_source_line(text)
	fmt = detect_format(text)
	if fmt == "bracket":
		entries = parse_bracket_format(text)
	elif fmt == "indented":
		entries = parse_indented_format(text)
	else:
		entries = []
	return merge_consecutive(entries)


def main():
	if len(sys.argv) != 2:
		print("Usage: python slack_parser.py <file>", file=sys.stderr)
		sys.exit(1)

	path = Path(sys.argv[1])
	if not path.exists():
		print(f"File not found: {path}", file=sys.stderr)
		sys.exit(1)

	text = path.read_text(encoding='utf-8')
	entries = parse_slack(text)
	output = {
		"entries": [
			{"body": e.body, "author": e.author}
			for e in entries
		]
	}
	print(json.dumps(output, ensure_ascii=False, indent=2))


if __name__ == '__main__':
	main()
