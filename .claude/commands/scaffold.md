---
description: "Generate feature directory structure with boilerplate"
---

# Feature Scaffold

Create a complete feature directory for `$ARGUMENTS` following project conventions.

## Steps

1. **Analyze** existing features in the codebase for patterns (directory structure, naming, exports).
2. **Generate** the feature directory with:
   - Main component/module file
   - Type definitions
   - Unit test file with at least 3 test cases (render, interaction, error state)
   - Index/barrel export
   - README with usage examples
3. **Create a feature branch**: `feature/$ARGUMENTS`
4. **Verify** the generated files pass linting and type checks.
5. **Stage** the generated files.

Follow existing patterns exactly. Do not invent new conventions.
