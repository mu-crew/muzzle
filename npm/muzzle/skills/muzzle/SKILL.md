---
name: muzzle
description: Per-host admission for heavy commands (gates, builds, test suites). Use when a bash result carries a muzzle line (queued, admitted, waited/ran, killed while queued), when wrapping a heavy command by hand with `muzzle run`, or when tuning muzzle's rule weights.
---

# muzzle

Placeholder: the skill_docs unit writes this skill and its recipes
(`recipes/tune.md`, `recipes/bench.md`, `recipes/harnesses.md`), spec §The skill.

Until then: never put `timeout` outside `muzzle run`; use
`muzzle run --run-timeout S -- CMD` instead.
