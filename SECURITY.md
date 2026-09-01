# Security Policy

## Supported versions

Security fixes are made for the latest published release. Older releases are not supported unless a release notice explicitly says otherwise. Before the first public release, reports against the default branch are handled on a best-effort basis.

## Report a vulnerability privately

Do not open a public issue for a suspected vulnerability. Use [GitHub private vulnerability reporting](https://github.com/bt-lang/bt/security/advisories/new) so the maintainers can investigate without exposing users before a fix is available.

Include as much of the following information as possible:

- The affected BT version, commit, target platform, and enabled Cargo features.
- A clear description of the impact and the conditions required to reproduce it.
- Minimal reproduction steps or a proof of concept.
- Relevant logs, stack traces, or package metadata with credentials and personal data removed.
- Any known workaround or suggested remediation.

The maintainers will review the report, ask for additional information when needed, and coordinate remediation and disclosure with the reporter. Please allow time for a fix to be developed and validated before publishing technical details.

## Scope

Reports about the BT runtime, CLI, desktop runtime, extension loaders, release artifacts, and repository automation are in scope. Vulnerabilities in third-party dependencies are also useful when they affect a supported BT build or reachable runtime path.

General support requests, feature proposals, and non-security bugs should use the repository issue templates instead.
