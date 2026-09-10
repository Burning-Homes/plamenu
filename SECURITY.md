# Security policy

## Supported versions

Only the latest published experimental release receives security fixes. These
releases may require a fresh installation.

## Reporting a vulnerability

Do not open a public issue. Email `security@burning.homes` with:

- affected version and deployment shape;
- impact and required attacker access;
- reproduction steps or proof of concept;
- suggested mitigation, if known;
- your disclosure preferences and a safe way to contact you.

Please avoid accessing data you do not own, disrupting federated services, or
publishing details before a fix and advisory are available. Good-faith research
that follows these constraints will not be pursued by the project.

## Release handling

Confirmed vulnerabilities receive a private fix, regression test, new signed
release, and public advisory containing impact, affected versions, mitigation,
and credit where requested. Secrets must never be sent through public CI logs or
issues.
