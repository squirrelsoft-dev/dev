---
description: "Create a pull request with full context"
---

# Pull Request Generator

Create a comprehensive PR for the current branch.

## Steps

1. **Diff analysis** — Run `git diff main...HEAD` to understand all changes.
2. **Generate PR body:**
   - **What changed** — bullet points of key changes
   - **Why** — business context and motivation
   - **How to test** — step-by-step verification instructions
   - **Test coverage** — what tests were added/modified
   - **Breaking changes** — if any
3. **Generate title** — conventional format: `type(scope): description`
4. **Create PR** — use `gh pr create --title "..." --body "..."` to open the PR.
5. **Link issues** — scan commits for issue references and include `Closes #N` in the body.
