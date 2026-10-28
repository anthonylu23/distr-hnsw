# Security policy

distr-hnsw is pre-release software. No version is supported for production
use yet, and the current build has known, documented security limitations.

## Known limitations (not vulnerabilities)

- Storage agents accept a request only with a short-lived grant signed by
  the portal's key, and a `DELETE` grant must name an applied
  garbage-collection proof. On a tailnet address an agent also requires the
  caller to be the portal node it pinned at join (Tailscale WhoIs on the
  socket; identity headers are ignored). WhoIs names a node, not a process:
  any process on the portal node that can read the master key can sign
  grants.
- The portal is a set of local commands; there is no network-facing portal
  service, browser session, or API key yet. Tailscale identity and
  authorization are milestone M2, in progress
  ([docs/m2-implementation-plan.md](docs/m2-implementation-plan.md)).

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
