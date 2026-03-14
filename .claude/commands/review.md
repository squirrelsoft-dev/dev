---
description: "Review staged or uncommitted changes"
---

# Code Review

Review the current changes for quality, bugs, and security issues.

## Steps

1. **Gather** — Run `git diff` (or `git diff --cached` if changes are staged).
2. **Analyze** each changed file for:
   - **Bugs** — logic errors, off-by-one, null handling, race conditions
   - **Security** — injection, auth bypasses, hardcoded secrets, XSS
   - **Performance** — N+1 queries, unnecessary re-renders, missing indexes
   - **Style** — naming, complexity, DRY violations
3. **Summarize** findings as:
   - 🔴 **Critical** — must fix before merge
   - 🟡 **Warning** — should fix, not a blocker
   - 🟢 **Suggestion** — nice to have
4. **Suggest** specific fixes for each critical and warning item.
