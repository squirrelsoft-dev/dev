#!/usr/bin/env bash
# PreCompact hook — saves context snapshot before compaction

mkdir -p .claude/context-snapshots

TIMESTAMP=$(date +%Y%m%d-%H%M%S)

{
  echo "# Context Snapshot — $TIMESTAMP"
  echo ""
  echo "## Current Branch"
  git branch --show-current 2>/dev/null
  echo ""
  echo "## Uncommitted Changes"
  git status --short
  echo ""
  echo "## Recent Activity"
  git log --oneline -10
} > ".claude/context-snapshots/snapshot-$TIMESTAMP.md"
