---
description: "Squash branch commits into one clean commit and open a PR"
---

# Squash & PR

Squash all commits on the current branch into a single conventional commit and open a pull request.

**Arguments:** `$ARGUMENTS` — optional issue number/identifier (e.g. `42`, `ORG-123`)

## Steps

1. **Detect base branch** — run `git branch -a` and determine whether the repo uses `main` or `master` as the default branch. Store as `<base>`.

2. **Detect git platform** — run `git remote get-url origin` and classify:
   - Contains `github.com` → **GitHub** (use `gh` CLI)
   - Contains `gitlab.com` or a self-hosted GitLab pattern → **GitLab** (use `glab` CLI)
   - Contains `bitbucket.org` → **Bitbucket** (limited CLI — provide manual URL)

3. **Resolve issue number** — if `$ARGUMENTS` is provided, use it as the issue identifier. Otherwise, extract from the current branch name using common patterns:
   - `fix/123-description` → `123`
   - `feature/ORG-123-description` → `ORG-123`
   - `issue-42` → `42`
   - If no issue number can be extracted, proceed without one.

4. **Fetch issue context** (if an issue number was resolved):
   - GitHub: `gh issue view <number>`
   - GitLab: `glab issue view <number>`
   - Bitbucket: skip (use branch name and commit context instead)

   Use the issue title and description to inform the commit message.

5. **Analyze branch commits** — run `git log --oneline <base>..HEAD` to understand the full scope of work done on this branch.

6. **Analyze the diff** — run `git diff <base>...HEAD` to understand exactly what changed.

7. **Generate conventional commit message** using all context gathered:

   ```
   type(scope): description (#issue)

   body — what changed and why, summarizing the full branch work

   Closes #issue
   ```

   Follow the same rules as `/commit`: imperative, lowercase description, no period. Scope is the primary module/feature affected.

8. **Squash commits** — run:
   ```
   git reset --soft $(git merge-base <base> HEAD)
   ```
   Then commit all staged changes with the generated message using `git commit -m "<message>"`.

9. **Force push** — run `git push --force-with-lease` to update the remote branch.

10. **Create PR** — platform-specific:
    - **GitHub:** `gh pr create --title "<type>(scope): description (#issue)" --body "<PR body>"`
    - **GitLab:** `glab mr create --title "<title>" --description "<body>"`
    - **Bitbucket:** print the manual PR creation URL: `https://bitbucket.org/<org>/<repo>/pull-requests/new?source=<branch>`

    The PR body should include:
    - **Summary** — bullet points of what changed
    - **Issue reference** — `Closes #<issue>` or platform equivalent
    - **Test plan** — how to verify the changes

    If a PR already exists for this branch, skip creation and print the existing PR URL instead.
