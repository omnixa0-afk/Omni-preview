# Security policy

Omni is a preview and has not been independently audited. Reports are very welcome.

## Reporting a vulnerability

Please **do not** open a public issue for a security problem. Use GitHub's private reporting:
**Security → Report a vulnerability** on this repository.

Include what you found, how to reproduce it, and which release or commit you tested. You will get an
answer as soon as possible; fixes are released and the report is published once users can update.

## Scope

In scope: the Rust core, the Windows app, the native media module, the wire protocol and the release
build pipeline. The model of what Omni does and does not defend against is in
[omni-threat-model.md](omni-threat-model.md); a gap between that document and the code is itself a
valid report.

Out of scope: anonymity against a global network observer (Omni is not an anonymity network) and
attacks that need malware already running as the same Windows user.

## Supported versions

Only the latest preview release gets fixes.
