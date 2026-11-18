# Security Policy

<p align="center">  [中文](SECURITY.md) · **English**  </p>

> Security vulnerability reporting and handling process for the ZhuYe
> project. This document is intended for all researchers, users, and
> contributors who want to disclose vulnerabilities responsibly.

---

## Supported Versions

The table below lists the ZhuYe versions currently receiving security updates.

| Version | Supported |
|---------|-----------|
| 0.0.1   | ✅ Yes    |
| < 0.0.1 | ❌ No     |

> All internal versions prior to 0.0.1 (internal codenames 0.1.x through
> 1.3) were never publicly released and are outside the scope of public
> support. Please upgrade to 0.0.1 or later.

---

## Reporting a Vulnerability

If you discover a security vulnerability in ZhuYe, **do not** submit it via GitHub Issues, Discussions,
or a public PR, to avoid malicious exploitation before a patch is released.

### Private Reporting Channels

Contact the maintainers privately through any of the following:

- **Email**: `security@zhuye-agent.dev`
- **Subject prefix**: `[SECURITY]` (to ease mail filtering)
- **Encryption** (optional): the maintainers' GPG public key is not
currently distributed with the repository; you may request it in your email

### Information to Include in a Report

To help us reproduce and fix the issue quickly, please provide as much of the following as possible:

1. **Vulnerability type and impact scope** (e.g., command injection,
information disclosure, permission bypass)
2. **Affected component / file / function / configuration item**
3. **Trigger conditions and minimal reproduction steps** (including full
commands, inputs, and configuration)
4. **Affected version** (the version number per the table above)
5. **Known mitigations** (if any, including temporary workarounds)
6. **Whether it has already been exploited in a public environment** (if
so, how far the impact extends)
7. **Your contact information** (so we can clarify details later)

### Our Commitments

- Acknowledge receipt within **72 hours** of getting the report
- Fix and release a patch within a reasonable time (typically 7–30 days,
depending on complexity)
- After the fix, disclose publicly via the
[GitHub Security Advisory](https://github.com/Asketisch/ZhuYe/security/advisories) (unless you explicitly
request anonymity)
- Credit the reporter (if you are willing to be named in the announcement)
- Disclose no details publicly before the fix

---

## Credentials and Privacy

**Never** include the following in issues, PRs, commit messages, Discussions, or chat groups:

- Any LLM provider API key (OpenAI, Anthropic, Ollama, etc.)
- Any GitHub / GitLab / platform personal access token (PAT)
- Any user's real name, email, IP address, or token
- Any session file content (which may contain sensitive context)

This project ships with the `secret-sanitize` tool, which automatically detects and redacts the following
common credential patterns:

- `sk-...`-style OpenAI / Anthropic / third-party platform keys
- `ghp_...` / `gho_...`-style GitHub tokens
- Bearer / Basic Authorization headers
- The `api_key` field in custom provider configuration

But **do not rely** on automatic detection — self-discipline is always the most reliable line of defense.

---

## Public Disclosure Policy

We follow the [coordinated vulnerability
disclosure](https://en.wikipedia.org/wiki/Coordinated_vulnerability_disclosure) principle:

- Report received → acknowledged → investigated → fixed → public
disclosure timing coordinated
- Reporters are given a default **90-day** disclosure window (negotiable
to extend or shorten)
- If the fix has not landed and there is no progress within 90 days, the
reporter may disclose publicly on their own (we continue to encourage collaboration)

---

## Security-Related Updates

- Follow this repository's [GitHub Releases](https://github.com/Asketisch/ZhuYe/releases)
and [Security Advisories](https://github.com/Asketisch/ZhuYe/security/advisories) for security
announcements
- Upgrading to the latest version is the simplest way to avoid known
vulnerabilities

---

## Acknowledgments

Thanks to all the researchers and users who disclose vulnerabilities responsibly and contribute to
ZhuYe's security.

> This policy is written with reference to
> [GitHub Security Advisories](https://docs.github.com/en/code-security/security-advisories),
> [Apache 2.0 security disclosure best practices](https://www.apache.org/security/),
> and the [CNCF Security TAG](https://github.com/cncf/tag-security).
