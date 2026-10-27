# Architecture Decision Records

This directory records decisions where the Rust port deliberately departs from the Python implementation, or chooses between designs that the Python code does not settle. Python's ADRs live in its own repository under `docs/adr/`; this directory has its own numbering.

## Convention

The format is the same lightweight MADR the Python project uses.

- **Path and naming:** `docs/adr/NNNN-kebab-title.md`, numbered from `0001` and zero-padded to four digits.
- **Required headings, in this order:** `## Status`, `## Context`, `## Decision`, `## Consequences`.
- **`## Decision` opens with a line beginning `**Ruling:**`**, which states the decision in one sentence.
- **`## Status`** is `Accepted`, `Superseded by NNNN`, or `Deprecated`, followed by the date it was set (for example `Accepted — 2026-10-26`).

When an ADR departs from Python, add the difference to the matching "Known ... differences from Python" list in `docs/PROJECT_PLAN.md` as well.

## Index

| ADR | Ruling |
|---|---|
| [0001 — The interactive interface renders inline, in Claude Code's style](0001-inline-terminal-interface.md) | Stage 9's interface renders inline (native scrollback plus a live region at the bottom) with ratatui and crossterm, in Claude Code's visual style. It keeps Python's commands, messages, approvals, files, and cancellation, and covers every Python screen in stages. |
