---
description: "Analyze and fix a GitHub issue end-to-end"
---

# Fix GitHub Issue

Resolve issue `$ARGUMENTS` from analysis through PR creation.

## Steps

1. **Fetch** — `gh issue view $ARGUMENTS` to get the full issue context.
2. **Analyze** — Identify affected files, root cause, and reproduction steps.
3. **Branch** — Create `fix/issue-$ARGUMENTS` from latest main.
4. **Implement** — Make the minimal fix following existing code patterns.
5. **Test** — Write a regression test that fails without the fix and passes with it.
6. **Verify** — Run the full test suite. Ensure no regressions.
7. **Commit** — Use conventional commit format referencing the issue.
8. **PR** — Create a PR that auto-closes the issue with full context.

Keep the change minimal. Fix the bug, add the test, nothing else.
