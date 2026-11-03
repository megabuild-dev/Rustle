# Security policy

Rustle runs as root-adjacent infrastructure: it controls Docker, holds your deploy tokens and encrypted environment
variables, and sits in front of your sites. Security reports are taken seriously and handled before anything else.

## Reporting a vulnerability

**Do not open a public issue.** Use GitHub's private reporting instead:

1. Go to the repository's **Security** tab, then **Report a vulnerability**
   (<https://github.com/megalabs-dev/rustle/security/advisories/new>).
2. Describe what you found, how to reproduce it, and the impact you expect.

You will get a first answer within **3 working days**. We aim to ship a fix within **30 days** for high-severity issues
and to credit you in the advisory unless you prefer to stay anonymous.

## Supported versions

Rustle is pre-1.0. Only the latest release and the `main` branch receive security fixes.

| Version | Supported |
|---|---|
| latest release / `main` | yes |
| older releases | no |

## What is in scope

* Anything that lets an unauthenticated party reach the admin API, a site's deploy webhook, or the Docker API.
* Escaping a tenant container, or reading another site's environment variables.
* Breaking the encryption of backups, environment variables or registry tokens.
* Accepting an unsigned or tampered self-update.
* Weaknesses in the installer that could be abused by a network attacker.

The design assumptions, trust boundaries and known gaps are written down in [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md).
Known limitations listed there (for example that sites on the same Docker network can reach each other) are tracked
openly and are not treated as new vulnerabilities, although a concrete exploit on top of one is welcome.

## Out of scope

* Vulnerabilities in your own applications, in Docker, Caddy or the operating system (report those upstream).
* Findings that need root access on the server already.
* Denial of service by a user who holds a valid admin session.

## How releases are protected

* Release binaries are signed with an ed25519 key; the running server verifies the signature and a manifest that binds
  version, architecture and SHA-256 before it installs an update ([docs/RELEASING.md](docs/RELEASING.md)).
* Dependencies are checked for known advisories and incompatible licences on every pull request
  (`cargo deny`, see `deny.toml`) and updated by Dependabot.
