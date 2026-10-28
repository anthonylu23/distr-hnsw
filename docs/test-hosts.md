# Test hosts

Evidence in these docs names hosts by role. Two machines produced it.

| Role | Hardware and system | Used for |
|---|---|---|
| Reference host | Fedora 44 desktop; AMD Ryzen 7 5800X (8 cores, 16 threads); 15 GiB RAM with 8 GiB zram swap; NVIDIA RTX 3060 Ti (8 GiB); WD_BLACK SN7100 NVMe with btrfs `/home`; kernel `7.1.9-200.fc44` for the 2026 runs | Drills (power loss, capacity, restore, lifecycle matrix), engine benchmarks, phase-0 bake-offs, GPU embedding with Ollama |
| Development laptop | MacBook on macOS with APFS | Development and small fixture tests |

Paths such as `~/distr-hnsw-bench/` and `~/distr-hnsw-drill/` are relative
to the home directory of the account that runs the scripts on the reference
host. Scripts that reach the reference host from the laptop take its SSH
destination from an environment variable (`REMOTE=user@host`).

Results from the reference host are not a supported-hardware statement:
they show what one commodity desktop measured. Repeat the drills on your own
hardware before relying on them (see [deployment.md](deployment.md)).
