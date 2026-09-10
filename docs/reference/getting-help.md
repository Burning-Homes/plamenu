# Getting help

For operator questions, check the [operator guide](../operators/index.md) and
[troubleshooting notes](../operators/troubleshooting.md), then ask in the
[project repository](https://codefloe.com/plamenu/plamenu).

## Report a problem or propose a change

- **Bug:** include the Plamenu version, installation method, reproduction steps,
  expected and actual behavior, and relevant logs. Redact credentials, tokens,
  and private content.
- **Federation problem:** use the interoperability issue template. Identify both
  server versions, which server sent the activity, which received it, and what
  happened on each side.
- **Feature proposal:** explain the user problem and how the proposed behavior
  would help. Mention effects on other clients or servers where relevant.
- **Security vulnerability:** follow the [security policy](security.md) and report
  privately. Do not open a public issue.

## What to expect

Community help is best-effort. Experimental releases do not guarantee production
availability, data recovery, upgrade or downgrade paths, exact Mastodon behavior,
or private operational support.

If you use a modified image or a database outside the documented setup, include
those differences in your report. You may be asked to reproduce the problem with
the release binary or image and the documented PostgreSQL version.

Changing an established ActivityPub domain can break remote references to accounts
and posts. Missing HTTPS or incorrect reverse-proxy settings can cause login and
federation failures. Check these against the
[configuration guide](../operators/configuration.md) when diagnosing a deployment.
