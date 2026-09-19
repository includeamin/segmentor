#!/usr/bin/env bash
# Writes release notes in Markdown from Conventional Commits.
#
# Usage: changelog.sh <from> <to> [repository-url]
#   from  exclusive start ref (a tag), or "" for the whole history
#   to    inclusive end ref
#   repository-url  e.g. https://github.com/owner/repo, used for commit and compare links
#
# Sections: Breaking changes, Features, Bug fixes, Performance, Documentation, Refactoring,
# Build and CI, Tests, Other changes. Merge commits are skipped.
set -euo pipefail

from="${1-}"
to="${2:?usage: changelog.sh <from> <to> [repository-url]}"
repo="${3-}"

range="$to"
[[ -n "$from" ]] && range="$from..$to"

declare -A sections=()
breaking=""
conventional_re='^([A-Za-z]+)(\(([^)]*)\))?(!)?:\ (.+)$'
breaking_re=$'(^|\n)BREAKING[ -]CHANGE:[ ]*([^\n]*)'

link() {
  local hash="$1" short="${1:0:7}"
  if [[ -n "$repo" ]]; then printf '[`%s`](%s/commit/%s)' "$short" "$repo" "$hash"; else printf '`%s`' "$short"; fi
}

while IFS= read -r -d $'\x1e' record; do
  record="${record#"${record%%[![:space:]]*}"}"
  [[ -z "$record" ]] && continue
  hash="${record%%$'\x1f'*}"
  rest="${record#*$'\x1f'}"
  subject="${rest%%$'\x1f'*}"
  body="${rest#*$'\x1f'}"

  key="other"
  scope=""
  description="$subject"
  is_breaking=0
  if [[ "$subject" =~ $conventional_re ]]; then
    type="${BASH_REMATCH[1],,}"
    scope="${BASH_REMATCH[3]}"
    description="${BASH_REMATCH[5]}"
    [[ -n "${BASH_REMATCH[4]}" ]] && is_breaking=1
    case "$type" in
      feat) key=feat ;;
      fix) key=fix ;;
      perf) key=perf ;;
      docs) key=docs ;;
      refactor) key=refactor ;;
      build | ci) key=build ;;
      test) key=test ;;
      *) key=other ;;
    esac
  fi
  note=""
  if [[ "$body" =~ $breaking_re ]]; then
    is_breaking=1
    note="${BASH_REMATCH[2]}"
  fi

  line="- "
  [[ -n "$scope" ]] && line+="**$scope:** "
  line+="$description ($(link "$hash"))"
  sections[$key]+="$line"$'\n'
  if ((is_breaking)); then
    entry="$line"
    [[ -n "$note" ]] && entry+=$'\n'"  - $note"
    breaking+="$entry"$'\n'
  fi
done < <(git log --no-merges --format='%H%x1f%s%x1f%b%x1e' "$range")

emit_section() {
  local title="$1" content="$2"
  [[ -z "$content" ]] && return 0
  printf '### %s\n\n%s\n' "$title" "$content"
}

emitted=0
if [[ -n "$breaking" ]]; then
  emit_section "⚠ Breaking changes" "$breaking"
  emitted=1
fi
for entry in "feat:Features" "fix:Bug fixes" "perf:Performance" "docs:Documentation" \
  "refactor:Refactoring" "build:Build and CI" "test:Tests" "other:Other changes"; do
  key="${entry%%:*}"
  title="${entry#*:}"
  if [[ -n "${sections[$key]-}" ]]; then
    emit_section "$title" "${sections[$key]}"
    emitted=1
  fi
done
((emitted == 0)) && printf 'No user-facing changes.\n\n'

if [[ -n "$repo" && -n "$from" ]]; then
  printf '**Full changelog**: %s/compare/%s...%s\n' "$repo" "$from" "$to"
fi
