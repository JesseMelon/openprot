<!-- Licensed under the Apache-2.0 license -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Repo agent context

## Git conventions (repo-wide)

- **Do NOT add `Co-Authored-By: Claude …` trailers** to commit messages.
- **Do NOT add "🤖 Generated with Claude Code"** (or similar attribution) to
  commit messages or PR bodies.

Pointers to scoped, auto-loaded context. Read the relevant one before
working in that area.

- **i2c userspace driver** — [`drivers/i2c/CLAUDE.md`](drivers/i2c/CLAUDE.md):
  status (master-only milestone; slave/target + notifications still required
  for the ocp-emea demo), locked decisions, crate map, build/test commands,
  working guardrails. Read this before any `drivers/i2c`, `target/*/backend/i2c`,
  or i2c board/init work.
