# distr-hnsw

[![CI](https://github.com/anthonylu23/distr-hnsw/actions/workflows/ci.yml/badge.svg)](https://github.com/anthonylu23/distr-hnsw/actions/workflows/ci.yml)

Self-hosted, Tailscale-native distributed semantic storage: files replicated
as encrypted, content-addressed objects across machines you already own,
indexed by meaning with distributed HNSW vector search.

> **Status: pre-release, not ready for data you cannot lose.** The blob plane
> and a single-partition vector engine are implemented and accepted against
> their milestone gates. Tailscale identity, the distributed vector plane,
> the extraction pipeline, and the dashboard are not built yet. See
> [docs/roadmap.md](docs/roadmap.md).

## Milestones

| Milestone | State |
|---|---|
| M0 Semantic validation | Accepted |
| M1 Blob plane and recovery foundation | Accepted (re-accepted 2026-10-28 after the [hardening pass](docs/hardening-2026-10.md)) |
| M2 Tailscale identity and authorization | Not started |
| M3 Single-partition vector engine | Accepted (2026-10-28, after the [hardening pass](docs/hardening-2026-10.md)) |
| M4–M7 Distributed vector plane, extraction, dashboard, vault and release | Not started |

## Build and test

Requires rustup and a C compiler (for `aws-lc-sys`). The Rust version is
pinned in `rust-toolchain.toml`, which rustup installs on first use.

```sh
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/minio-test.sh        # S3 backup tests against a throwaway MinIO
```

Tests write temporary data under `target/` so durability tests run on a real
filesystem. CI runs the format check, clippy (also with `--features
failpoints`), and the test suite on every push and pull request. Larger
drills (restore, lifecycle matrix, power loss, ENOSPC) and the benchmarks
are in [scripts/](scripts/README.md) and run on a dedicated host
([docs/test-hosts.md](docs/test-hosts.md)).

## Documentation

- [docs/architecture.md](docs/architecture.md): system overview
- [DESIGN.md](DESIGN.md): product and architecture specification
- [docs/deployment.md](docs/deployment.md): running the current build, and
  its limitations
- [docs/roadmap.md](docs/roadmap.md): milestones, acceptance criteria, and
  evidence
- Contracts: [blob storage](docs/m1-storage-contract.md),
  [lifecycle](docs/m1-lifecycle-contract.md),
  [vector engine](docs/m3-engine-contract.md)

## Security

See [SECURITY.md](SECURITY.md). In the current build, agents require a
portal-signed grant on every request, and the portal is local commands only.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state
otherwise, any contribution intentionally submitted for inclusion in this
work, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
