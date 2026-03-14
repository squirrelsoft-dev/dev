#!/usr/bin/env bash
# PostToolUse format hook — detects the project formatter and formats the changed file

INPUT=$(cat)
FILE_PATH=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty')

# Nothing to format if no file path
[[ -z "$FILE_PATH" ]] && exit 0

# Skip non-existent files (e.g. deleted)
[[ ! -f "$FILE_PATH" ]] && exit 0

# --- Node/JS/TS projects ---
if [[ -f "package.json" ]]; then
  # Biome: check package.json deps or biome.json config
  if [[ -f "biome.json" ]] || [[ -f "biome.jsonc" ]] || grep -q '"@biomejs/biome"' package.json 2>/dev/null; then
    npx @biomejs/biome format --write "$FILE_PATH" 2>/dev/null || true
    exit 0
  fi

  # Prettier: check package.json deps or config files
  if [[ -f ".prettierrc" ]] || [[ -f ".prettierrc.json" ]] || [[ -f ".prettierrc.js" ]] || [[ -f ".prettierrc.cjs" ]] || [[ -f ".prettierrc.mjs" ]] || [[ -f ".prettierrc.yaml" ]] || [[ -f ".prettierrc.yml" ]] || [[ -f ".prettierrc.toml" ]] || [[ -f "prettier.config.js" ]] || [[ -f "prettier.config.cjs" ]] || [[ -f "prettier.config.mjs" ]] || grep -q '"prettier"' package.json 2>/dev/null; then
    npx prettier --write "$FILE_PATH" 2>/dev/null || true
    exit 0
  fi

  # Fallback for Node projects: try prettier then biome
  npx prettier --write "$FILE_PATH" 2>/dev/null || npx @biomejs/biome format --write "$FILE_PATH" 2>/dev/null || true
  exit 0
fi

# --- Python projects ---
if [[ -f "pyproject.toml" ]] || [[ -f "requirements.txt" ]] || [[ -f "setup.py" ]]; then
  ruff format "$FILE_PATH" 2>/dev/null || black "$FILE_PATH" 2>/dev/null || true
  exit 0
fi

# --- Rust projects ---
if [[ -f "Cargo.toml" ]]; then
  rustfmt "$FILE_PATH" 2>/dev/null || true
  exit 0
fi

# --- Go projects ---
if [[ -f "go.mod" ]]; then
  gofmt -w "$FILE_PATH" 2>/dev/null || true
  exit 0
fi
