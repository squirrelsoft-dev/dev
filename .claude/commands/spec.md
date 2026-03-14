---
description: "Generate detailed specs from a task breakdown"
---

# Spec Generator

Generate detailed specification files from a task breakdown.

**Input**: `$ARGUMENTS` is the name of a task file (maps to `.claude/tasks/$ARGUMENTS.md`).

## Workflow

1. Read the task list file from `.claude/tasks/$ARGUMENTS.md`. If it doesn't exist, tell the user and suggest running `/breakdown $ARGUMENTS` first.
2. Parse all tasks — items matching the pattern `- [ ] **Task title**`.
3. Create the output directory `.claude/specs/$ARGUMENTS/`.
4. Use `TeamCreate` to create a team named `spec-$ARGUMENTS`.
5. For each task, spawn an agent using the `Task` tool (subagent_type: `general-purpose`) with `team_name` set to `spec-$ARGUMENTS`. Run agents in parallel where possible.
6. Each agent receives:
   - The task title
   - The task description (lines following the checkbox until the next task)
   - The listed files
   - Dependency info (Blocked by / Blocking)
   - The full path to write its spec file: `.claude/specs/$ARGUMENTS/{task-title-kebab}.md`
7. Each agent must:
   - Read the files listed in the task to understand existing code
   - Search for relevant community skills by running `npx skills find <topic>` for the key technologies or patterns in the task. If a useful skill is found, run `npx skills add <owner/repo@skill>` to install it and note it in the spec under Implementation Details.
   - Write a spec file to `.claude/specs/$ARGUMENTS/{task-title-kebab}.md` using the format below
8. After all agents complete, list the generated spec files for the user.

## Spec File Format

Each agent should produce a file with this exact structure:

```markdown
# Spec: <Task Title>
> From: .claude/tasks/{name}.md

## Objective
<What this task accomplishes and why>

## Current State
<Relevant existing code/architecture — agent reads the listed files>

## Requirements
- <Specific, testable requirements>

## Implementation Details
- Files to create/modify with descriptions of changes
- Key functions/types/interfaces to add
- Integration points with existing code

## Dependencies
- Blocked by: <tasks that must complete first>
- Blocking: <tasks that depend on this>

## Risks & Edge Cases
- <Potential issues and mitigations>

## Verification
- <How to confirm this task is done correctly>
```

## Rules

- Do NOT implement any code — only produce spec files
- Each spec must be grounded in the actual codebase (agents must read the listed files)
- Use kebab-case for spec filenames derived from task titles
- After all specs are written, print a summary of all generated files
