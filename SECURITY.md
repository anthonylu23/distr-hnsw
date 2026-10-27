# Security policy

distr-hnsw is pre-release software. No version is supported for production
use yet, and the current build has known, documented security limitations.

## Known limitations (not vulnerabilities)

- Storage agents have no authentication. They bind to loopback only and
  accept any request from the local host, including physical `DELETE`.
  Tailscale identity and authorization are milestone M2
  ([docs/roadmap.md](docs/roadmap.md)).
- The portal is a set of local commands; there is no network-facing portal
  service yet.

See [docs/deployment.md](docs/deployment.md) for the full list of limitations
a deployment must carry.

## Reporting a vulnerability

Report suspected vulnerabilities privately through GitHub's private
vulnerability reporting on this repository (Security → Report a
vulnerability). Do not open a public issue. Please include:

- the affected component (agent, portal, engine, backup) and revision;
- the impact, such as data loss, data disclosure, integrity, or key
  material;
- steps to reproduce.

Issues that break durability or recovery invariants (acknowledged data
lost, deleted data resurrected, corrupt data served without detection) are
treated as security issues even without an attacker.
