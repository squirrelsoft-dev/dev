---
description: "Pick the next task grouping from a breakdown and implement it with an agent team"
---

# Work Next Task

Pick a task grouping from a task breakdown and spin up an agent team to implement it, guided by the specs.

**Input:** `$ARGUMENTS` — optional task list name (maps to `.claude/tasks/$ARGUMENTS.md`). If omitted, the user picks from available task lists.

## Steps

### 1. Resolve the task list

- If `$ARGUMENTS` is provided, read `.claude/tasks/$ARGUMENTS.md`. If it doesn't exist, tell the user and stop.
- If `$ARGUMENTS` is **not** provided:
  1. Glob for all files matching `.claude/tasks/*.md`.
  2. For each file, read the first heading line (`# Task Breakdown: ...`) and check whether any tasks remain incomplete (lines matching `- [ ]`). Exclude files where every task is `- [x]`.
  3. If no task lists have incomplete tasks, tell the user "All task lists are complete" and stop.
  4. Present the list to the user with `AskUserQuestion` — show each task list name, its heading, and a count of remaining tasks (e.g. "my-feature — 4 tasks remaining"). Let the user pick one.

### 2. Parse task groupings

Read the selected task list file and parse its structure:

- Groups are headed by `## Group N — <label>` lines.
- Tasks within a group match the pattern `- [ ] **Task title**` (incomplete) or `- [x] **Task title**` (complete).
- A group is **fully complete** if all its tasks are `- [x]`.
- A group is **available** if it is not fully complete and all groups it depends on (listed in the `_Depends on: ..._` line) are fully complete.
- A group is **blocked** if it depends on a group that still has incomplete tasks.

### 3. Let the user choose a task grouping

Use `AskUserQuestion` to present the available (non-blocked, non-complete) groups. For each group show:
- Group number and label
- Count of incomplete tasks in the group
- Task titles at a glance

If only one group is available, skip the question and auto-select it.

If no groups are available (all remaining groups are blocked), tell the user which groups are blocking and stop.

### 4. Verify specs exist

Check that `.claude/specs/$ARGUMENTS/` exists and contains spec files. For each incomplete task in the selected group, look for a matching spec file at `.claude/specs/<task-list-name>/<task-title-kebab>.md`.

- If specs are missing for any task, tell the user which specs are missing and suggest running `/spec <task-list-name>` first. Stop.

### 5. Create a feature branch

Create and switch to a branch for this work:
```
git checkout -b work/<task-list-name>-group-<N>
```
If the branch already exists, switch to it instead.

### 6. Plan and build the agent team

Switch to **plan mode** (`EnterPlanMode`). In the plan:

1. Read each spec file for the tasks in the selected group from `.claude/specs/<task-list-name>/`.
2. Identify dependencies between tasks — tasks within the group are parallel by design, but note any ordering preferences from `Blocked by` / `Blocking` fields in the specs.
3. Search for relevant community skills by running `npx skills find <topic>` for key technologies or patterns across the group's tasks. If useful skills are found, run `npx skills add <owner/repo@skill>` to install them before spawning agents.
4. For each task, define an agent assignment:
   - **Agent name** — derived from the task title (kebab-case)
   - **Agent type** — `general-purpose`
   - **Prompt** — include the full spec content, the list of files to create/modify, and explicit instructions to implement the spec (not just plan). Include instructions to search for and install skills when encountering unfamiliar libraries or patterns (see Skills CLI below).
5. Present the plan to the user for approval via `ExitPlanMode`.

### 7. Execute the team

After plan approval:

1. Use `TeamCreate` to create a team named `work-<task-list-name>-group-<N>`.
2. For each task, use `TaskCreate` to add it to the team's task list.
3. Spawn agents in parallel using the `Task` tool with `team_name` set to the team name. Each agent:
   - Reads its assigned spec file
   - If the task involves an unfamiliar library or pattern, searches for a skill first: `npx skills find <topic>`, then `npx skills add <owner/repo@skill>` if a match is found
   - Implements the changes described in the spec
   - Runs any verification steps listed in the spec (tests, lint, type checks)
   - Reports completion
4. As agents complete, monitor for failures. If an agent fails, report the issue to the user.

### 8. CRITICAL — Mark tasks complete in the task list file

> **You MUST complete this step. Do not skip it. The task list file is the source of truth for progress.**

After all agents in the group finish successfully:

1. Read the task list file at `.claude/tasks/<task-list-name>.md`.
2. For **every** task that was implemented in this group, find the line `- [ ] **Task title**` and replace it with `- [x] **Task title**`.
3. Write the updated file back using the `Edit` tool (one edit per task, or read-then-write the whole file).
4. **Verify** — re-read the file and confirm all implemented tasks now show `- [x]`. If any were missed, fix them.

This step is **not optional**. If this step is skipped, `/work` will re-select already-completed tasks on the next run.

### 9. Summary

Print a summary:
- Which tasks were completed
- Files created or modified (aggregate from agent results)
- Which groups are now unblocked for the next `/work` run
- Confirm the task list file was updated (print the path)
- Suggest the user review changes with `git diff` and then run `/squash-pr` when ready

## Skills CLI

When a task involves a library, framework, or pattern you're not confident about, use the skills CLI to find and install community skills that provide expert guidance.

```bash
# Search for skills by topic
npx skills find <topic>

# Install a skill from the results
npx skills add <owner/repo@skill>
```

Include these instructions in every agent prompt so teammates can discover and install skills during implementation.

## Rules

- Do NOT skip the user selection steps — always let the user confirm which task list and group to work on (unless there's only one option).
- Do NOT implement tasks that belong to other groups.
- Each agent must follow its spec — the spec is the source of truth for implementation.
- If a spec is ambiguous or incomplete, the agent should make reasonable choices consistent with existing codebase patterns.
- **ALWAYS update the task list file** (step 8) after work is done — this is mandatory, not a suggestion.
