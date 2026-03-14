#!/usr/bin/env bash
# PreToolUse guard — blocks destructive commands before execution

INPUT=$(cat)
CMD=$(echo "$INPUT" | jq -r '.tool_input.command // empty')

# Block destructive patterns
if echo "$CMD" | grep -qEi '(rm -rf /|DROP TABLE|DROP DATABASE|truncate |DELETE FROM .* WHERE 1|force push|--force|> /dev/sd|mkfs|dd if=)'; then
  echo '{"decision":"block","reason":"Blocked: destructive command detected. Rephrase with a safer approach."}'
  exit 0
fi

# Block secret leaks
if echo "$CMD" | grep -qEi '(curl.*(-d|--data).*(_KEY|_SECRET|_TOKEN|PASSWORD)|echo.*(_KEY|_SECRET|_TOKEN))'; then
  echo '{"decision":"block","reason":"Blocked: potential secret exposure. Use environment variables, not inline secrets."}'
  exit 0
fi

echo '{"decision":"approve"}'
