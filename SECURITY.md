# Security policy

## Reporting a vulnerability

Please report security issues **privately** through GitHub's
[private vulnerability reporting](https://github.com/Hymetdotfun/hymet-distributor/security/advisories/new) for this repository.
Do not open a public issue, and do not test against Mainnet funds you do not own.

Include what is affected (program instruction, API route, keeper step), the impact, and steps to reproduce. We will
acknowledge the report, keep you informed, and credit you if you wish once a fix is released.

## Scope

- The HYMET distributor program `9X9fnj4Uh9WC9nmxCahTVsixHZ6VGxNpMesuGhK239Px`
- For the web application, keeper and tooling, report in [Hymetdotfun/hymet](https://github.com/Hymetdotfun/hymet/security/advisories/new)

Meteora's DBC and DAMM v2 programs, Phantom, and Solana itself are out of scope; please report issues in those
to their maintainers.

## Current status

- Distributor: internal security review, reproducible build ([docs/VERIFY-BUILD.md](docs/VERIFY-BUILD.md)); no
  external audit yet.
- The program is upgradeable by `9G9v3Ax4K2prmVQfDzp2HY6YYZ5UFmCo24761pgEmMo7`; upgrades are public on-chain and change
  the published binary hash.
