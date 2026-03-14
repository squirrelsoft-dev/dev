---
paths:
  - "**/*.test.*"
  - "**/*.spec.*"
  - "**/__tests__/**"
  - "tests/**"
---

# Testing Rules

- Test behavior, not implementation details
- Include edge cases: empty inputs, null values, boundary conditions
- Use descriptive test names that explain what is being verified
- One assertion per test when possible
- Mock external services, never hit real APIs in tests
