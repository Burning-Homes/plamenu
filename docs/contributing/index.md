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

Check links and syntax for prose and example edits. `./dev docs-check` also
builds the application and checks runtime documentation. If CLI help changes, regenerate it with
`./dev docs-generate`. Shell examples in `docs/examples/` must pass syntax
checking and ShellCheck; deployment TOML examples must pass configuration tests.
