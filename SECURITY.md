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
- The portal service (`portal serve`) identifies each connection with
  Tailscale WhoIs and admits only browser sessions and scoped API keys over
  HTTPS. It trusts `tailscaled` on its own host: whoever controls that
  daemon or its LocalAPI socket controls identity. Sign-in takes the
  identity WhoIs reports, so any process on a user's devices can sign in
  as that user (the portal's own node is refused by default); tailnet
  ACLs decide which devices reach the portal. Tailscale identity and
  authorization are milestone M2, in progress
  ([docs/m2-implementation-plan.md](docs/m2-implementation-plan.md)).
- The local `portal` commands are not authenticated: anyone who can read the
  portal database and the master key operates the cluster, and anyone who
  can write the database can add users and keys.

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
