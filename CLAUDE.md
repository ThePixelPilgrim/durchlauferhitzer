# durchlauferhitzer

## Model roles (mandatory)

- **Fable never does implementation work itself.** When the session runs on
  Fable, it may brainstorm, write specs and plans, coordinate, and review —
  but it must not write or edit product code, tests, build files, or
  dependencies. All implementation is delegated to subagents running on
  Opus or Sonnet (Agent tool with `model: "opus"` or `model: "sonnet"`).
- This applies to every execution path, including
  `subagent-driven-development` and `executing-plans`: under Fable, "Native"
  execution is not allowed; use subagent-driven execution with Opus/Sonnet
  implementers.

## Skills

The superpowers skills are vendored as project skills in `.claude/skills/`
(see `.claude/skills/SUPERPOWERS-SOURCE.md`). Where a skill refers to
`superpowers:<name>`, use the project skill `<name>`.
