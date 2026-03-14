#!/usr/bin/env bash
# Stop hook — generates audit trail of what was done in this session

mkdir -p .claude/logs

TIMESTAMP=$(date +%Y%m%d-%H%M%S)
BRANCH=$(git branch --show-current 2>/dev/null || echo "unknown")

{
  echo "# Task Completed — $TIMESTAMP"
  echo "Branch: $BRANCH"
  echo ""
  echo "## Files Changed"
  git diff --stat HEAD~1 2>/dev/null || git status --short
  echo ""
  echo "## Recent Commits"
  git log --oneline -5 2>/dev/null
} > ".claude/logs/task-$TIMESTAMP.md" 2>/dev/null
