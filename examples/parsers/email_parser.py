import argparse
from dateutil.parser import parse as parse_date
import json
import re
import sys
from collections import Counter
from pathlib import Path


from_pattern = re.compile(r'^From:\s+\S')
header_keywords = re.compile(r'^(Date|Sent|To|Cc|Subject):')


def is_header_block(lines, idx):
	for i in range(idx + 1, min(idx + 7, len(lines))):
		if header_keywords.match(lines[i]):
			return True
	return False


def is_blank(line):
	return re.sub(r'[\s\u200b\xa0\uE000-\uF8FF]', '', line) == ''


def normalize_line(line):
	return re.sub(r'[\s\u200b\xa0]+', ' ', line).strip().lower()


def clean_body(lines):
	result = []
	for line in lines:
		cleaned = line.rstrip().replace('\u200b', '').replace('\xa0', ' ').replace('\u202f', ' ')
		result.append(cleaned)
	while result and is_blank(result[0]):
		result.pop(0)
	while result and is_blank(result[-1]):
		result.pop()
	result = '\n'.join(result)
	result = re.sub(r'\n{3,}', '\n\n', result)
	return result


def clean_author(name):
	name = re.sub(r'\s*<[^>]+>\s*$', '', name).strip()
	match = re.match(r"^'(.+?)'\s+via\s+.+", name)
	if match:
		name = match.group(1)
	return name.strip()


def extract_author_from_from_line(line):
	match = re.match(r'^From:\s+"([^"]+)"\s*<', line)
	if match:
		return clean_author(match.group(1))
	match = re.match(r'^From:\s+(.+?)\s*<', line)
	if match:
		return clean_author(match.group(1))
	return clean_author(re.sub(r'^From:\s+', '', line))


def normalize_timestamp(raw):
	cleaned = raw.replace('\u202f', ' ').replace(' at ', ' ').strip()
	hour_match = re.search(r' (\d{2}):\d{2} ?PM$', cleaned)
	if hour_match and int(hour_match.group(1)) > 12:
		cleaned = re.sub(r' ?PM$', '', cleaned)
	try:
		return parse_date(cleaned).strftime('%Y-%m-%dT%H:%M:%S')
	except (ValueError, OverflowError):
		return raw.strip()


def extract_timestamp(line):
	match = re.match(r'^(?:Date|Sent):\s+(.+)$', line)
	if match:
		return normalize_timestamp(match.group(1))
	return None


def parse_top_message(lines):
	first_zwsp_idx = None
	for i, line in enumerate(lines):
		if '\u200b' in line:
			first_zwsp_idx = i
			break

	if first_zwsp_idx is not None:
		author = None
		for j in range(first_zwsp_idx - 1, -1, -1):
			if not is_blank(lines[j]):
				author = lines[j].strip()
				break

		body_start = first_zwsp_idx
		while body_start < len(lines):
			if is_blank(lines[body_start]) or '\u200b' in lines[body_start]:
				body_start += 1
			else:
				break
	else:
		# no recipients block — first non-blank line is author
		author = None
		body_start = 0
		for i, line in enumerate(lines):
			if not is_blank(line):
				if author is None:
					author = line.strip()
				else:
					body_start = i
					break

	body = clean_body(lines[body_start:])
	if author:
		return {"author": clean_author(author), "timestamp": None, "body": body}
	return None


def parse_quoted_reply(lines):
	if not lines:
		return None

	author = extract_author_from_from_line(lines[0])
	timestamp = None
	subject_idx = None

	for i, line in enumerate(lines[1:], 1):
		if line.startswith('Date:') or line.startswith('Sent:'):
			timestamp = extract_timestamp(line)
		if line.startswith('Subject:'):
			subject_idx = i
			break

	body_start = (subject_idx + 1) if subject_idx is not None else 1
	body = clean_body(lines[body_start:])
	return {"author": author, "timestamp": timestamp, "body": body}


def parse_email_chain(text, striplines=None):
	lines = text.split('\n')
	if not lines:
		return []

	start = 0
	if lines[0].startswith('# source:'):
		start = 1

	from_indices = []
	for i in range(start, len(lines)):
		if from_pattern.match(lines[i]) and is_header_block(lines, i):
			from_indices.append(i)

	entries = []

	top_end = from_indices[0] if from_indices else len(lines)
	top_entry = parse_top_message(lines[start:top_end])
	if top_entry:
		entries.append(top_entry)

	for idx, from_idx in enumerate(from_indices):
		next_from = from_indices[idx + 1] if idx + 1 < len(from_indices) else len(lines)
		reply_entry = parse_quoted_reply(lines[from_idx:next_from])
		if reply_entry:
			entries.append(reply_entry)

	if striplines:
		for entry in entries:
			body_lines = entry['body'].split('\n')
			filtered = [l for l in body_lines if normalize_line(l) not in striplines]
			entry['body'] = re.sub(r'\n{3,}', '\n\n', '\n'.join(filtered)).strip()

	entries.reverse()
	return entries


def generate_candidates(directory):
	counter = Counter()
	for path in sorted(Path(directory).glob('*.txt')):
		text = path.read_text(encoding='utf-8')
		entries = parse_email_chain(text)
		for entry in entries:
			seen = set()
			for line in entry['body'].split('\n'):
				normalized = normalize_line(line)
				if normalized and normalized not in seen:
					seen.add(normalized)
					counter[normalized] += 1

	for line, count in counter.most_common():
		print(f"{count}\t{line}")


junk_path = Path(__file__).resolve().parent / 'junk.txt'


def load_striplist():
	if not junk_path.exists():
		return None
	lines = junk_path.read_text(encoding='utf-8').split('\n')
	result = set()
	for line in lines:
		line = line.strip()
		if not line or line.startswith('#'):
			continue
		if '\t' in line:
			line = line.split('\t', 1)[1]
		normalized = normalize_line(line)
		if normalized:
			result.add(normalized)
	return result if result else None


def check_results(directory):
	issues = []
	for path in sorted(Path(directory).glob('*.txt')):
		text = path.read_text(encoding='utf-8')
		entries = parse_email_chain(text)
		name = path.name

		if not entries:
			issues.append((name, "NO_ENTRIES", "produced zero entries"))
			continue

		for i, entry in enumerate(entries):
			author = entry['author']
			body = entry['body']
			timestamp = entry['timestamp']

			if not author:
				issues.append((name, f"entry[{i}]", "null author"))
			elif '<' in author or '@' in author:
				issues.append((name, f"entry[{i}]", f"email in author: {author!r}"))
			elif len(author) > 60:
				issues.append((name, f"entry[{i}]", f"long author ({len(author)}): {author[:80]!r}"))
			elif len(author) < 2:
				issues.append((name, f"entry[{i}]", f"short author: {author!r}"))
			elif not re.search(r'[a-zA-Z]', author):
				issues.append((name, f"entry[{i}]", f"non-alpha author: {author!r}"))

			if not body or not body.strip():
				issues.append((name, f"entry[{i}]", "empty body"))
			elif len(body) > 5000:
				issues.append((name, f"entry[{i}]", f"long body ({len(body)} chars)"))

			if timestamp is not None and not re.match(r'\d{4}-\d{2}-\d{2}T', timestamp):
				issues.append((name, f"entry[{i}]", f"unparsed timestamp: {timestamp!r}"))

	if not issues:
		print("No issues found.")
	else:
		for filename, location, message in issues:
			print(f"{filename}\t{location}\t{message}")


def main():
	parser = argparse.ArgumentParser()
	parser.add_argument('input', help='file or directory path')
	parser.add_argument('--generate-candidates', action='store_true')
	parser.add_argument('--check', action='store_true')
	args = parser.parse_args()

	if args.generate_candidates:
		generate_candidates(args.input)
	elif args.check:
		check_results(args.input)
	else:
		text = Path(args.input).read_text(encoding='utf-8')
		striplines = load_striplist()
		entries = parse_email_chain(text, striplines)
		json.dump({"entries": entries}, sys.stdout, indent=2)
		print()


if __name__ == '__main__':
	main()
