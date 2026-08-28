---
name: public-readme
description: Use whenever editing README.md or any public-facing text (crate description, --help output, install/usage docs). Keeps internal status, undecided decisions, release blockers, and author checklists out of public surfaces.
---

# README is a landing page, not a status board

README.md (and every public-facing surface: Cargo.toml `description`,
`--help` text, install instructions) is read by strangers deciding whether
and how to use this project. It is NOT a checklist, a planning document, or
a place to confess what the author has not done yet.

## Never put these in README

- Undecided decisions ("repository owner not yet decided", "license TBD",
  "whether to publish is a release-time decision").
- Release blockers, TODOs, "not yet" confessions, verification debt.
- Owner/author deliberations or decision history ("removed by owner
  decision", "the author will…").
- Internal development state (parameterized-until-decided workflows,
  `publish = false` guard rationale, private environments).
- Anything phrased as a note-to-self or note-to-maintainer.

## Where that content goes instead

- **docs/PLAN.md** — decisions (numbered "Key decisions"), open items
  ("Deliberately open"), milestones. This is the internal status board.
- **docs/** chapters — deep technical rationale for contributors.

When removing internal content from README, MOVE it to the right home if
it is not already recorded there — never silently delete an open item.

## How to phrase the public version

- Planned work is stated simply and confidently, without exposing what
  blocks it:
  - ✗ "Release blocker: binary releases require the GitHub repository
    location, which is not yet decided."
  - ✓ "Binary releases are planned; for now, build from source."
- Absent features are framed as scope, not as unfinished work:
  - ✗ "Per-user attribution is not implemented yet."
  - ✓ "What it deliberately does not do: per-user attribution."
- No links from README to internal status sections; link to docs/ chapters
  only when they help a user or contributor.

## Checklist before finishing any README edit

1. Read every changed sentence as a stranger evaluating the project: does
   it help them understand, install, or use it? If it only informs the
   author, move it.
2. Grep README for leak markers: `TBD`, `not yet`, `undecided`, `blocker`,
   `pending`, `decision`, `owner`. Each hit must justify itself.
3. Standard public sections are fine and stay: features, install, usage,
   configuration, compatibility, development quick-start, license
   (including the dual-license contribution clause).
4. Never let internal cleanup weaken honesty toward users: real
   limitations that affect USERS (e.g. "percentiles are estimates",
   "history caps at ~68 min in memory") stay in README — they are product
   documentation, not internal status.
