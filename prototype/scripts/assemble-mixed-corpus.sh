#!/usr/bin/env bash
# Copy-only assembly of a phase-0 mixed corpus onto anthonypc.
# Source trees are never modified. Every invocation uses an empty stage so
# stale files from an earlier assembly cannot affect evaluation.
set -euo pipefail

REMOTE="${REMOTE:-anthonylu@anthonypc}"
CORPUS_ID="${CORPUS_ID:-mixed-$(date -u +%Y%m%dT%H%M%SZ)}"
STAGE_REMOTE="${STAGE_REMOTE:-/home/anthonylu/distr-hnsw-proto/corpora/$CORPUS_ID}"
META_REMOTE="${META_REMOTE:-$(dirname "$STAGE_REMOTE")}"
LAPTOP_DOCS="${LAPTOP_DOCS:-$HOME/Documents}"
LAPTOP_PROJECTS="${LAPTOP_PROJECTS:-$HOME/Documents/Projects}"

RSYNC_FILTERS=(
  --exclude '.git/'
  --exclude '.conductor/'
  --exclude 'node_modules/'
  --exclude 'target/'
  --exclude 'dist/'
  --exclude '.next/'
  --exclude '.venv/'
  --exclude 'venv/'
  --exclude '__pycache__/'
  --exclude '*.pyc'
  --exclude '.env'
  --exclude '.env.*'
  --exclude 'credentials.json'
  --exclude '*.pem'
  --exclude '*.key'
  --exclude '.DS_Store'
  --exclude '.obsidian/'
  --exclude 'site-packages/'
  --exclude 'prototype/testdata/'
  --include '*/'
  --include '*.[tT][xX][tT]'
  --include '*.[mM][dD]'
  --include '*.[mM][aA][rR][kK][dD][oO][wW][nN]'
  --include '*.[rR][sS]'
  --include '*.[pP][yY]'
  --include '*.[tT][sS]'
  --include '*.[tT][sS][xX]'
  --include '*.[jJ][sS]'
  --include '*.[jJ][sS][xX]'
  --include '*.[gG][oO]'
  --include '*.[jJ][aA][vV][aA]'
  --include '*.[cC]'
  --include '*.[hH]'
  --include '*.[cC][pP][pP]'
  --include '*.[hH][pP][pP]'
  --include '*.[hH][tT][mM][lL]'
  --include '*.[hH][tT][mM]'
  --include '*.[cC][sS][sS]'
  --include '*.[jJ][sS][oO][nN]'
  --include '*.[yY][aA][mM][lL]'
  --include '*.[yY][mM][lL]'
  --include '*.[tT][oO][mM][lL]'
  --include '*.[cC][sS][vV]'
  --include '*.[pP][dD][fF]'
  --exclude '*'
)

SEEN_PDF_HASHES="$(mktemp -t distr-hnsw-pdf-hashes.XXXXXX)"
trap 'rm -f "$SEEN_PDF_HASHES"' EXIT

copy_unique_pdf() {
  local src="$1"
  local dest="$2"
  local digest
  digest="$(shasum -a 256 "$src" | awk '{print $1}')"
  if grep -Fqx "$digest" "$SEEN_PDF_HASHES"; then
    echo "skip duplicate PDF content: $src"
    return 0
  fi
  rsync -a "$src" "$REMOTE:$dest"
  printf '%s\n' "$digest" >> "$SEEN_PDF_HASHES"
}

rsync_to() {
  local src="$1"
  local dest="$2"
  shift 2
  if [[ ! -e "$src" ]]; then
    echo "skip missing source: $src" >&2
    return 0
  fi
  ssh "$REMOTE" "mkdir -p $(printf %q "$dest")"
  if (($#)); then
    rsync -a --stats "$@" "${RSYNC_FILTERS[@]}" "$src" "$REMOTE:$dest"
  else
    rsync -a --stats "${RSYNC_FILTERS[@]}" "$src" "$REMOTE:$dest"
  fi
}

echo "==> requiring empty stage on $REMOTE:$STAGE_REMOTE"
stage_q="$(printf %q "$STAGE_REMOTE")"
ssh "$REMOTE" "
  if [[ -d $stage_q ]] && [[ -n \$(find $stage_q -mindepth 1 -print -quit) ]]; then
    echo 'refusing non-empty stage: $STAGE_REMOTE' >&2
    exit 2
  fi
  mkdir -p \
  $(printf %q "$STAGE_REMOTE")/{notes/anthonypc-vault,notes/laptop-vault,code,pdfs/{psych,cs,learning,resumes,misc},public/{gutenberg,html,pdf,code}}"

echo "==> copying laptop Obsidian notes"
rsync_to "$LAPTOP_DOCS/obsidianVault/" "$STAGE_REMOTE/notes/laptop-vault/"

echo "==> copying small code projects (supported text/code only via full tree + excludes)"
for proj in beli-cli switchboard-cli personal-website distr-hnsw; do
  rsync_to "$LAPTOP_PROJECTS/$proj/" "$STAGE_REMOTE/code/$proj/"
done

echo "==> copying Learning cpp notes/code/pdfs"
rsync_to "$LAPTOP_DOCS/Learning/cpp/" "$STAGE_REMOTE/code/learning-cpp/" --exclude '*.pdf'

echo "==> copying PSYCH PDFs"
shopt -s nullglob
psych_pdfs=("$LAPTOP_DOCS/PSYCH_study_materials/"*.pdf)
if ((${#psych_pdfs[@]})); then
  for f in "${psych_pdfs[@]}"; do
    copy_unique_pdf "$f" "$STAGE_REMOTE/pdfs/psych/"
  done
else
  echo "skip missing PSYCH PDFs" >&2
fi

echo "==> copying Learning PDFs"
learning_pdfs=("$LAPTOP_DOCS/Learning/cpp/"*.pdf)
if ((${#learning_pdfs[@]})); then
  for f in "${learning_pdfs[@]}"; do
    copy_unique_pdf "$f" "$STAGE_REMOTE/pdfs/learning/"
  done
else
  echo "skip missing Learning PDFs" >&2
fi
shopt -u nullglob

echo "==> copying resume PDFs (flattened)"
ssh "$REMOTE" "mkdir -p $(printf %q "$STAGE_REMOTE/pdfs/resumes")"
if [[ -d "$LAPTOP_DOCS/Resumes-Latest" ]]; then
  while IFS= read -r -d '' f; do
    copy_unique_pdf "$f" "$STAGE_REMOTE/pdfs/resumes/"
  done < <(find "$LAPTOP_DOCS/Resumes-Latest" -iname '*.pdf' -type f -print0)
else
  echo "skip missing resume directory" >&2
fi

echo "==> copying Homework PDF"
if [[ -f "$LAPTOP_DOCS/Homework 3 Report.pdf" ]]; then
  copy_unique_pdf \
    "$LAPTOP_DOCS/Homework 3 Report.pdf" \
    "$STAGE_REMOTE/pdfs/misc/"
fi

echo "==> copying up to 50 CS PDFs (excluding venv/site-packages)"
ssh "$REMOTE" "mkdir -p $(printf %q "$STAGE_REMOTE/pdfs/cs")"
if [[ -d "$LAPTOP_DOCS/CS" ]]; then
  cs_pdf_count=0
  while IFS= read -r f; do
    digest="$(shasum -a 256 "$f" | awk '{print $1}')"
    if grep -Fqx "$digest" "$SEEN_PDF_HASHES"; then
      echo "skip duplicate PDF content: $f"
      continue
    fi
    copy_unique_pdf "$f" "$STAGE_REMOTE/pdfs/cs/"
    cs_pdf_count=$((cs_pdf_count + 1))
    if ((cs_pdf_count >= 50)); then
      break
    fi
  done < <(
    find "$LAPTOP_DOCS/CS" -iname '*.pdf' -type f \
      ! -path '*/.venv/*' ! -path '*/site-packages/*' ! -path '*/sklearn-env/*' \
      -print | LC_ALL=C sort
  )
else
  echo "skip missing CS directory" >&2
fi

echo "==> copying anthonypc-local sources (vault + cuda course)"
ssh "$REMOTE" "STAGE='$STAGE_REMOTE' bash -s" <<'EOF'
set -euo pipefail
copy_supported() {
  local src="$1"
  local dest="$2"
  mkdir -p "$dest"
  (
    cd "$src"
    find . -type f \( \
      -iname '*.txt' -o -iname '*.md' -o -iname '*.markdown' -o -iname '*.rs' \
      -o -iname '*.py' -o -iname '*.ts' -o -iname '*.tsx' -o -iname '*.js' \
      -o -iname '*.jsx' -o -iname '*.go' -o -iname '*.java' -o -iname '*.c' \
      -o -iname '*.h' -o -iname '*.cpp' -o -iname '*.hpp' -o -iname '*.html' \
      -o -iname '*.htm' -o -iname '*.css' -o -iname '*.json' -o -iname '*.yaml' \
      -o -iname '*.yml' -o -iname '*.toml' -o -iname '*.csv' -o -iname '*.pdf' \
    \) \
      ! -path './.git/*' ! -path './.conductor/*' ! -path './.obsidian/*' \
      ! -path './node_modules/*' \
      ! -path './target/*' ! -path './dist/*' ! -path './.venv/*' \
      -print0 | rsync -a --from0 --files-from=- ./ "$dest/"
  )
}
if [[ -d "$HOME/Documents/Obsidian Vault" ]]; then
  copy_supported "$HOME/Documents/Obsidian Vault" "$STAGE/notes/anthonypc-vault"
fi
if [[ -d "$HOME/Documents/cuda-freecodecamp" ]]; then
  copy_supported "$HOME/Documents/cuda-freecodecamp" "$STAGE/code/cuda-freecodecamp"
fi
EOF
echo "==> downloading public fillers on remote"
ssh "$REMOTE" "STAGE=$(printf %q "$STAGE_REMOTE") META=$(printf %q "$META_REMOTE") bash -s" <<'REMOTE_EOF'
set -euo pipefail
CORPUS_ID="${STAGE##*/}"
PUB="$STAGE/public"
mkdir -p "$PUB"/{gutenberg,html,pdf,code}
cd "$PUB"

SOURCES="$META/${CORPUS_ID}-SOURCES.md"
cat > "$SOURCES" <<'HDR'
# Public filler sources for the phase-0 mixed corpus

Copied into `public/` for phase-0 coverage. Licenses are as published by the
upstream hosts. Private laptop/anthonypc copies live outside `public/`.
Meta files (this SOURCES list, inventory, manifest, and queries) live beside
the run-scoped corpus, not inside it, so they are not indexed.

| Path | URL | Notes |
|---|---|---|
HDR

dl() {
  local url="$1"
  local out="$2"
  local note="$3"
  if [[ -f "$out" ]]; then
    echo "keep existing $out"
  else
    echo "fetch $url -> $out"
    curl -fsSL --retry 3 --retry-delay 2 -o "$out" "$url"
  fi
  printf '| `%s` | %s | %s |\n' "${out#$STAGE/}" "$url" "$note" >> "$SOURCES"
}

# Project Gutenberg plaintexts (public domain in the US)
dl "https://www.gutenberg.org/files/11/11-0.txt" \
  "$PUB/gutenberg/alice-in-wonderland.txt" "Public domain"
dl "https://www.gutenberg.org/files/1661/1661-0.txt" \
  "$PUB/gutenberg/adventures-of-sherlock-holmes.txt" "Public domain"

# HTML docs
dl "https://doc.rust-lang.org/book/ch01-02-hello-world.html" \
  "$PUB/html/rust-book-hello-world.html" "MIT / Apache-2.0 (Rust book)"
dl "https://developer.mozilla.org/en-US/docs/Web/API/Fetch_API" \
  "$PUB/html/mdn-fetch-api.html" "CC-BY-SA MDN"
dl "https://docs.python.org/3/tutorial/introduction.html" \
  "$PUB/html/python-tutorial-introduction.html" "PSF license"

# Public PDFs
dl "https://www.irs.gov/pub/irs-pdf/fw9.pdf" \
  "$PUB/pdf/irs-fw9.pdf" "US government work"
dl "https://arxiv.org/pdf/1706.03762.pdf" \
  "$PUB/pdf/attention-is-all-you-need.pdf" "arXiv preprint (CC BY / author terms)"
dl "https://arxiv.org/pdf/1810.04805.pdf" \
  "$PUB/pdf/bert-paper.pdf" "arXiv preprint (CC BY / author terms)"

# Tiny public GitHub snapshot (source + README only)
CODE_ZIP="$PUB/code/example-python.zip"
if [[ ! -d "$PUB/code/example-python" ]]; then
  curl -fsSL --retry 3 -o "$CODE_ZIP" \
    "https://codeload.github.com/python/cpython/zip/refs/heads/main"
  # Prefer a tiny repo instead if cpython is huge — use hello-world style
  rm -f "$CODE_ZIP"
  curl -fsSL --retry 3 -o "$CODE_ZIP" \
    "https://codeload.github.com/octocat/Hello-World/zip/refs/heads/master"
  unzip -q -o "$CODE_ZIP" -d "$PUB/code"
  # normalize directory name
  if [[ -d "$PUB/code/Hello-World-master" ]]; then
    mv "$PUB/code/Hello-World-master" "$PUB/code/example-python"
  fi
  rm -f "$CODE_ZIP"
fi
printf '| `%s` | %s | %s |\n' \
  "public/code/example-python" \
  "https://github.com/octocat/Hello-World" \
  "Public domain sample repo" >> "$SOURCES"

# Extra small public README-heavy tree via shallow contents of a tiny MIT repo
TINY_ZIP="$PUB/code/express-hello.zip"
if [[ ! -d "$PUB/code/node-fetch-readme" ]]; then
  curl -fsSL --retry 3 -o "$TINY_ZIP" \
    "https://codeload.github.com/sindresorhus/is-docker/zip/refs/heads/main"
  unzip -q -o "$TINY_ZIP" -d "$PUB/code"
  if [[ -d "$PUB/code/is-docker-main" ]]; then
    mv "$PUB/code/is-docker-main" "$PUB/code/node-fetch-readme"
  fi
  rm -f "$TINY_ZIP"
fi
printf '| `%s` | %s | %s |\n' \
  "public/code/node-fetch-readme" \
  "https://github.com/sindresorhus/is-docker" \
  "MIT" >> "$SOURCES"

# Inventory and a content manifest live beside the corpus so neither is indexed.
INV="$META/${CORPUS_ID}-INVENTORY.txt"
{
  echo "$CORPUS_ID inventory generated $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo
  echo "Total files: $(find "$STAGE" -type f | wc -l)"
  echo
  echo "By extension:"
  find "$STAGE" -type f -printf '%f\n' \
    | awk -F. 'NF > 1 { print tolower($NF) } NF == 1 { print "(none)" }' \
    | sort | uniq -c | sort -rn
  echo
  echo "By top-level bucket:"
  for b in notes code pdfs public; do
    echo "  $b: $(find "$STAGE/$b" -type f 2>/dev/null | wc -l)"
  done
  echo
  echo "Supported-ish extensions (prototype extractor):"
  find "$STAGE" -type f \( \
    -iname '*.md' -o -iname '*.txt' -o -iname '*.pdf' -o -iname '*.html' -o -iname '*.htm' \
    -o -iname '*.rs' -o -iname '*.py' -o -iname '*.ts' -o -iname '*.tsx' -o -iname '*.js' \
    -o -iname '*.jsx' -o -iname '*.go' -o -iname '*.java' -o -iname '*.c' -o -iname '*.h' \
    -o -iname '*.cpp' -o -iname '*.hpp' -o -iname '*.css' -o -iname '*.json' -o -iname '*.yaml' \
    -o -iname '*.yml' -o -iname '*.toml' -o -iname '*.csv' -o -iname '*.markdown' \
  \) | wc -l
} > "$INV"

MANIFEST="$META/${CORPUS_ID}-MANIFEST.sha256"
(
  cd "$STAGE"
  find . -type f -print0 | sort -z | xargs -0 sha256sum
) > "$MANIFEST"
MANIFEST_DIGEST="$META/${CORPUS_ID}-MANIFEST.digest"
sha256sum "$MANIFEST" > "$MANIFEST_DIGEST"

echo "Wrote $SOURCES"
echo "Wrote $INV"
echo "Wrote $MANIFEST"
echo "Wrote $MANIFEST_DIGEST"
cat "$INV"
cat "$MANIFEST_DIGEST"
REMOTE_EOF

echo "==> assemble-mixed-corpus done: $REMOTE:$STAGE_REMOTE"
echo "    Put queries beside the stage, e.g. ${STAGE_REMOTE}-queries.json"
echo "    (not inside the stage, so prepare does not index the query set)."
