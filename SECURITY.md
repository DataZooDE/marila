# Security Policy

## Supported Versions

marila is pre-1.0 software. Security fixes target the `main` branch until
the project starts cutting supported releases.

## Reporting a Vulnerability

Please do not open a public issue for a suspected vulnerability. Report it
privately to the repository maintainers through GitHub Security Advisories.

Include:

- Affected commit or version
- Reproduction steps
- Expected and observed behavior
- Any logs, requests, or payloads needed to validate the issue

## Current Security Model

marila is designed for local development and compatibility testing. It is
not production hardened:

- SigV4 headers are parsed but signatures are not verified.
- IAM, bucket policies, encryption policy enforcement, and per-request
  scoped credentials are not implemented.
- Docker Compose credentials are development defaults and must not be used
  for exposed deployments.

Do not expose a default marila deployment to untrusted networks.
