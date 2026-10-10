# Security Policy

## Supported versions

gauge is pre-1.0 and has no long-term-support branches. Security fixes go into
the latest release only.

| Version | Supported |
|---|---|
| latest `0.x` release | ✅ |
| anything older | ❌ |

## What gauge is — and isn't

gauge is a **measurement tool**, not a hardened network service. By design:

- `gauge server` has **no authentication or access control**: anyone who can
  reach its port can start a test and use its bandwidth.
- TCP and UDP tests are **not encrypted**.
- QUIC tests are encrypted, but the server uses a throw-away self-signed
  certificate and the client **does not verify it**, so a QUIC test does not
  authenticate the server.

These are documented behavior (see the README's *Security notes*), not
vulnerabilities. Run servers only on networks you trust, bind them to a
specific interface with `--bind` where possible, and stop them when you are
done.

## Reporting a vulnerability

Please **do not open a public issue** for a security problem.

Report it privately instead, using GitHub's
[private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
(the repository's **Security** tab → **Report a vulnerability**).

Helpful details:

- the affected version (`gauge --version`) and platform,
- what an attacker needs (network access to a server, a crafted client, a
  malicious server, local file access …) and what they gain,
- steps or a script to reproduce it, and ideally a suggested fix.

You can expect an acknowledgement within a few days. Fixes are developed
privately, released, and then disclosed in the [CHANGELOG](CHANGELOG.md) under
*Security*, with credit if you want it.

## Areas of particular interest

Anything where untrusted input can do more than waste bandwidth, for example:

- malformed handshakes or UDP datagrams that crash, hang or exhaust the memory
  of a server,
- a hostile server crashing or misleading a client beyond the documented
  trust model,
- path or content injection through the config, history or log files,
- dependency issues in the TLS/QUIC stack (these are also checked by
  `cargo deny` / `cargo audit` in CI).
