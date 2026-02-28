---
name: Bug Report
about: Report a bug or unexpected behavior
title: '[Bug] '
labels: ['bug']
assignees: ''
---

## Description

A clear and concise description of the bug.

## Steps to Reproduce

1. Configuration used (remove sensitive values):
   ```toml
   # paste relevant config sections here
   ```

2. Command run:
   ```bash
   adaclaw ...
   ```

3. Steps to reproduce the behavior:
   - Step 1
   - Step 2
   - ...

## Expected Behavior

What you expected to happen.

## Actual Behavior

What actually happened. Include error messages and logs:

```
# Paste error output here
```

## Environment

- AdaClaw version: (run `adaclaw --version`)
- OS: (Linux x86_64 / macOS aarch64 / Windows x86_64 / ...)
- Rust version: (run `rustc --version`, if building from source)
- Provider: (openai / anthropic / openrouter / ollama / ...)
- Channel: (telegram / discord / cli / ...)

## `adaclaw doctor` Output

<details>
<summary>Click to expand</summary>

```
# Paste output of `adaclaw doctor` here
```
</details>

## Additional Context

Any other context about the problem (config snippets with secrets removed, screenshots, etc.).
