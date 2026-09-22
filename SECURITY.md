# Security

This project is experimental. Do not put production credentials, request
bodies, private network configuration or user data in public issues.

Use GitHub's **Security → Report a vulnerability** for sensitive reports.
Include the affected revision, a minimal reproduction with synthetic data,
and the impact. For ordinary bugs, open an issue with secrets removed.

Only the latest main branch is maintained; there is no security-response SLA.

## Known dependency advisories (2026-09-22)

The pinned Valence `0.2.0-alpha.1` network dependency brings older HTTP/TLS and
RSA crates. `cargo audit` currently reports these advisories:

| Dependency | Advisory | Upstream constraint |
| --- | --- | --- |
| h2 0.3.27 | [RUSTSEC-2026-0258](https://rustsec.org/advisories/RUSTSEC-2026-0258.html) | Fixed in the 0.4 series; Valence uses reqwest 0.11 / hyper 0.14. |
| rsa 0.7.2 | [RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071.html) | No patched version listed in the advisory. |
| rustls-webpki 0.101.7 | [RUSTSEC-2026-0104](https://rustsec.org/advisories/RUSTSEC-2026-0104.html), [RUSTSEC-2026-0098](https://rustsec.org/advisories/RUSTSEC-2026-0098.html), [RUSTSEC-2026-0099](https://rustsec.org/advisories/RUSTSEC-2026-0099.html) | Fixes require the newer TLS dependency family. |

The server selects `ConnectionMode::Offline`; it does not use Valence's online
account-authentication flow. This reduces the relevance of that flow's HTTP/TLS
and RSA operations, but is not a proof that all vulnerable code is unreachable.
The repository is published for private-network experimentation and review,
not public hosting. Game players are not authenticated; MCP bearer auth protects
MCP only. Keep listeners local or within an appropriately restricted private
network. Do not enable online mode or expose the game/bridge publicly without
reviewing and updating the dependency chain.

The audit findings are not suppressed. Moving to a compatible maintained Valence
network stack and re-running protocol/browser tests is outstanding work.
