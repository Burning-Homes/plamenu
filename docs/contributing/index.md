# Contributing

Use the [Codefloe repository](https://codefloe.com/plamenu/plamenu) for bug
reports and pull requests. Report vulnerabilities through
[the security policy](../reference/security.md).

Discuss large features, schema or federation changes, and new dependencies
first. A patch should explain the problem, resulting behavior, checks run,
and any effects on stored data, configuration, security, or interoperability.
Sign commits and include the DCO sign-off (`git commit -S -s`) and update `Unreleased` for user-visible changes.

See [development](../DEVELOPMENT.md) for setup and checks.

## Documentation

Documentation source is in `docs/`. Use `./dev docs-build` to build a local preview.

Write for the task and audience: member actions in `members/`, deployment and
recovery in `operators/`, wire contracts in API/federation references. Verify UI
labels, commands, defaults and permission claims against the matching source.
Explain prerequisites, steps, expected result and recovery where relevant. Link
to one canonical explanation instead of duplicating it.

Do not add agent session logs, dated progress diaries, acceptance transcripts or
feature-completion checklists to the handbook. Store run artifacts under ignored
`target/`; use issues for plans and the changelog for user-visible changes. Keep
future work explicitly separate from shipped behavior. Never turn a subset, a
skip or an old run into a current full-pass or accessibility claim.

Check links and syntax for prose and example edits. `./dev docs-check` also
builds the application and checks runtime documentation. If CLI help changes, regenerate it with
`./dev docs-generate`. Shell examples in `docs/examples/` must pass syntax
checking and ShellCheck; deployment TOML examples must pass configuration tests.
