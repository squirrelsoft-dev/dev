#!/usr/bin/env bash
# Stop hook — comprehensive quality gate
# Runs only on changed files. Gracefully skips missing tools.

CHANGED=$(git diff --name-only --diff-filter=ACM HEAD 2>/dev/null)
[ -z "$CHANGED" ] && echo '{"decision":"approve"}' && exit 0

ERRORS=""

# 1. Secret detection (changed files only)
if command -v gitleaks &>/dev/null; then
  SECRETS=$(gitleaks detect --no-git --source=. --verbose 2>&1 | grep -E "Secret|Finding")
  [ -n "$SECRETS" ] && ERRORS="$ERRORS\n## 🔑 Secrets Detected\n$SECRETS"
fi

# 2. Semgrep SAST (changed files only)
if command -v semgrep &>/dev/null; then
  SAST=$(echo "$CHANGED" | xargs semgrep --config=auto --quiet --json 2>/dev/null | jq -r '.results[] | "\(.path):\(.start.line) \(.check_id) - \(.extra.message)"' 2>/dev/null)
  [ -n "$SAST" ] && ERRORS="$ERRORS\n## 🛡️ Security Issues (Semgrep)\n$SAST"
fi

# 3. Type check (TypeScript projects)
if [ -f tsconfig.json ]; then
  TC=$(npx tsc --noEmit 2>&1)
  echo "$TC" | grep -q "error TS" && ERRORS="$ERRORS\n## ❌ Type Errors\n$(echo "$TC" | grep 'error TS' | head -20)"
fi

# 4. Tests
TESTS=$(cargo test 2>&1)
[ $? -ne 0 ] && ERRORS="$ERRORS\n## 🧪 Test Failures\n$(echo "$TESTS" | tail -20)"

# 5. Dependency audit (quick)
if command -v npm &>/dev/null && [ -f package.json ]; then
  AUDIT=$(npm audit --audit-level=high 2>&1)
  echo "$AUDIT" | grep -q "high\|critical" && ERRORS="$ERRORS\n## 📦 Vulnerable Dependencies\n$(echo "$AUDIT" | tail -10)"
fi

if [ -n "$ERRORS" ]; then
  REASON=$(echo -e "Fix before completing:\n$ERRORS" | jq -Rs .)
  echo "{\"decision\":\"block\",\"reason\":$REASON}"
else
  echo '{"decision":"approve"}'
fi
