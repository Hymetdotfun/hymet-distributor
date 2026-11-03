# Verify the deployed distributor

Anyone can confirm that the program running at `9X9fnj4Uh9WC9nmxCahTVsixHZ6VGxNpMesuGhK239Px` is built from this
repository.

| | |
|---|---|
| Program ID | `9X9fnj4Uh9WC9nmxCahTVsixHZ6VGxNpMesuGhK239Px` |
| ProgramData | `EvEn4nMki2tWkHzTNiqLHERvVZSvG4gAXnBb5ZsuV9DJ` |
| Binary | 509,112 bytes, SHA-256 `fda10de4da728442da4a42a63bb1f5d8df614e49f8421c656ee2ca67f3be3e7b` |
| Deployed | Slot 464931680 (2026-11-03) |
| Pinned in | `config/production.json` in [Hymetdotfun/hymet](https://github.com/Hymetdotfun/hymet) (`programSha256`, `programBytes`) |

## 1. Hash the on-chain program

```bash
solana program dump 9X9fnj4Uh9WC9nmxCahTVsixHZ6VGxNpMesuGhK239Px deployed.so -u mainnet-beta
sha256sum deployed.so
```

`program dump` returns the ProgramData bytes without trailing padding.

## 2. Rebuild from source

Toolchain: Solana CLI (Agave) **2.2.1**, which brings platform tools v1.44; Rust from `rust-toolchain.toml`. Build
inputs are all tracked: `Cargo.toml`, `Cargo.lock`, `Anchor.toml`, `rust-toolchain.toml` and
`programs/hymet-rewards-distributor/`.

```bash
sh -c "$(curl -sSfL https://release.anza.xyz/v2.2.1/install)"
git clone https://github.com/Hymetdotfun/hymet-distributor && cd hymet-distributor
cargo build-sbf --manifest-path programs/hymet-rewards-distributor/Cargo.toml
sha256sum target/deploy/hymet_rewards_distributor.so
```

Both hashes must equal `fda10de4…3e7b`. Independent clean builds on separate machines produce the identical binary.

## 3. Check authorities and configuration

```bash
solana program show 9X9fnj4Uh9WC9nmxCahTVsixHZ6VGxNpMesuGhK239Px -u mainnet-beta
# Authority: 9G9v3Ax4K2prmVQfDzp2HY6YYZ5UFmCo24761pgEmMo7
```

The config account `5QMrhkg36hnJVBubARNLrL8vPaCb9koRYLeffVc8GNHG` (seeds `["config"]`) holds, after its 8-byte
discriminator: admin `9G9v…Mo7`, platform wallet `7sAo…qPEs` and snapshot authority `GHZH…oL8S`.

## 4. Run the program's tests

```bash
cargo test --manifest-path programs/hymet-rewards-distributor/Cargo.toml
```

These include the canonical DAMM v2 pool vectors (checked against Meteora's SDK), the fee split's conservation of
every base unit, Merkle leaf compatibility with the TypeScript planner, and the role separation of keeper rotation.
