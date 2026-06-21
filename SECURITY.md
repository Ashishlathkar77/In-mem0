# Security Policy

## Reporting a vulnerability

Please report security issues privately via GitHub's "Report a vulnerability" (Security Advisories)
on this repository, rather than opening a public issue. We aim to acknowledge within 72 hours.

Include: affected version/commit, reproduction steps, and impact.

## Scope notes for operators

`inmemd` is a cache server. Until authentication and TLS land (tracked on the roadmap), treat it
like any unauthenticated datastore:

- **Do not expose it directly to untrusted networks.** Bind to localhost or a private interface
  (`--bind`), and put it behind a firewall / private subnet.
- The protocol parser bounds bulk/array sizes (512 MiB / 1M elements) to limit memory abuse, and
  malformed frames close the connection — but there is no per-client auth or rate limiting yet.

## Supported versions

Pre-1.0: only the latest `main` receives fixes.
