# Security policy

This project is an early preview. It has not completed an independent security
review or a production sandbox integration. The supported preview is
`0.1.0-alpha.1`; fixes may require upgrading to a later preview, with API changes.

Please [report a vulnerability privately](https://github.com/bryanhelmig/sandbox-egress/security/advisories/new).
Use that channel for suspected policy bypasses, identity confusion, DNS
rebinding, revocation failures, and other security issues. Avoid public issues
or pull requests for undisclosed vulnerabilities.

Include the affected version, a minimal local reproducer, expected behavior,
and observed behavior. Do not include credentials, production addresses,
payload contents, or third-party data. Please allow maintainers a reasonable
chance to investigate before public disclosure; this project has no guaranteed
response-time SLA.

The implemented claims and host responsibilities are documented in
[README.md](README.md) and [security invariants](docs/security-invariants.md).
