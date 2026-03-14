---
description: "Generate conventional commit message from staged changes"
---

# Commit Message Generator

Analyze the staged changes (`git diff --cached`) and generate a conventional commit message.

## Format

```
<type>(<scope>): <short description>

<body — what changed and why>

<footer — closes/fixes issues if applicable>
```

## Types
feat, fix, docs, style, refactor, test, chore, perf, ci, build

## Rules
- Scope is the primary module/feature affected
- Description is imperative, lowercase, no period
- Body explains what and why, not how
- Reference issue numbers if identifiable from branch name or diff context

Output ONLY the commit message, nothing else. Then run `git commit -m "<message>"`.
