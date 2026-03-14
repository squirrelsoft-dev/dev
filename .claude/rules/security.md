---
paths:
  - "src/**/*.{ts,tsx,js,jsx}"
---

# Security Rules

- Validate all inputs, especially from users and external APIs
- Never store passwords in plain text; use bcrypt with salt
- Use parameterized queries or ORM to prevent SQL injection
- Sanitize HTML/user content before rendering (DOMPurify or equivalent)
- Use CSRF tokens for state-changing form submissions
- Do not introduce packages with known vulnerabilities
- Never hardcode secrets, API keys, or tokens in source code
