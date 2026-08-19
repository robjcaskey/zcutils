#!/usr/bin/env bash
set -euo pipefail

DOC_FILE="${1:?Usage: $0 <doc file> <chart version>}"
CHART_VERSION="${2:?Usage: $0 <doc file> <chart version>}"

if ! [[ "${CHART_VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "chart version must be MAJOR.MINOR.PATCH (received: ${CHART_VERSION})"
  exit 1
fi

python3 - "$DOC_FILE" "$CHART_VERSION" <<'PY'
from pathlib import Path
import re
import sys

path = Path(sys.argv[1])
version = sys.argv[2]
text = path.read_text()

replacements = [
    (re.compile(r'(?m)^(\s*--version\s+")\d+\.\d+\.\d+(")'), rf'\g<1>{version}\g<2>'),
    (re.compile(r'(?m)^(\s*--set image\.tag=")\d+\.\d+\.\d+(")'), rf'\g<1>{version}\g<2>'),
    (re.compile(r'chart-v\d+\.\d+\.\d+'), f'chart-v{version}'),
    (re.compile(r'(?m)(-f chart_version=")\d+\.\d+\.\d+(")'), rf'\g<1>{version}\g<2>'),
    (re.compile(r'(?m)(-f app_image_tag=")\d+\.\d+\.\d+(")'), rf'\g<1>{version}\g<2>'),
    (re.compile(r'\(`\d+\.\d+\.\d+`\)'), rf'(`{version}`)'),
]

for pattern, replacement in replacements:
    text = pattern.sub(replacement, text)

path.write_text(text)
PY
