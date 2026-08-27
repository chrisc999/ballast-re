# Ballast Re — baUSD

[![CI](https://github.com/chrisc999/ballast-re/actions/workflows/ci.yml/badge.svg)](https://github.com/chrisc999/ballast-re/actions/workflows/ci.yml)
[![coverage](https://img.shields.io/badge/coverage-report-4c1)](https://chrisc999.github.io/ballast-re/)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

**baUSD** is a Soroban-native, appreciating-share vault token backed by reinsurance
yield. LPs deposit USDC on Stellar and mint baUSD; the share price is
`total_assets / total_shares`, where `total_assets` is an attested NAV that can move
**up or down**. There is **no rebasing, no peg, and no principal guarantee.**

> ⚠️ baUSD is not a stablecoin. NAV is attested from off-chain reinsurance treaty
> performance and can decrease. Not investment advice.

**Status:** deployed on Stellar **testnet** · contracts **unaudited** · not for production use.

## Demo — deposit → mint → redeem on Stellar testnet

![baUSD testnet demo: an LP deposits 50 USDC, mints 50 baUSD, then redeems for 50 USDC](media/demo.gif)

A real, on-chain run against the deployed testnet contracts (reproduce with
`./scripts/demo_testnet.sh`).

## Architecture (short version)

- **baUSD token** — a SEP-41-compatible Soroban token (OpenZeppelin `stellar-tokens`
  base). Mint/burn authority is held **exclusively** by the vault contract. No
  discretionary issuance.
- **Vault** — appreciating-share accounting (ERC-4626-equivalent). Implemented:
  `subscribe`, `request_redemption`, `claim_redemption`, `cancel_redemption`, guardian
  `pause`/`unpause`, a two-step admin handover (`propose_admin`/`accept_admin`),
  `set_notice_period`, timelocked governance-gated upgrade, and swappable role setters.
  Attested NAV: `update_nav` (m-of-n quorum, bounded delta, cadence floor, proof reference,
  downward moves allowed), `update_nav_extraordinary` for catastrophe writedowns, and
  `share_price` published for external price feeds. Planned: liquidity-sleeve management,
  redemption queue and caps, and allowlist enforcement.
- **Escrow is always reversible** — `request_redemption` moves baUSD into the vault, so
  `cancel_redemption` is gated on nothing but the holder's own signature (not pause, not
  compliance). A request that cannot be claimed can always be undone.
- **Roles** (separation of duties) — governance multisig (admin/upgrade), guardian
  (pause), an m-of-n attestation quorum (NAV), compliance authority (allowlist), treasury
  ops (sleeve). Designed as swappable addresses so mainnet multisig drops in later.
- **NAV is attested, not traded** — baUSD has no market, so its price is published from
  off-chain treaty performance rather than discovered. NAV can fall. Routine updates are
  bounded in size and frequency; anything larger needs governance as well as the quorum.
  Stale NAV blocks new deposits and never blocks exits.

## Toolchain (pinned)

| Tool | Version |
|---|---|
| Rust | 1.97.1 (`rust-toolchain.toml`) |
| wasm target | `wasm32v1-none` |
| soroban-sdk | 26.1.1 |
| OpenZeppelin stellar-tokens | 0.7.2 |
| stellar-cli | 27.1.0 |

Why soroban-sdk 26.x and not the latest 27.x: OpenZeppelin's audited `stellar-tokens`
base still requires `soroban-sdk ^26.1`, so we pin to the audited token rather than the
bleeding-edge SDK. `stellar-cli` 27.x builds and deploys these contracts fine.

## Quickstart

```bash
# 1. Toolchain (rust-toolchain.toml pins the exact version)
rustup show                      # installs the pinned toolchain + wasm target
cargo install --locked stellar-cli@27.1.0

# 2. Build + test
cargo test                       # native unit + integration tests
stellar contract build           # produces wasm32v1-none artifacts

# 3. Lint (matches CI)
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

## Repository layout

```
contracts/
  token/     baUSD SEP-41 token (OZ stellar-tokens base)
  vault/     appreciating-share vault
scripts/     deploy + demo scripts (testnet)
.github/     CI workflows
```

Mock USDC in tests is Soroban's built-in Stellar Asset Contract
(`register_stellar_asset_contract_v2`) — no separate mock crate.

## Testnet deployment

One command deploys mock USDC + vault + baUSD token and wires them:

```bash
./scripts/deploy_testnet.sh     # writes deploy/testnet.json
./scripts/demo_testnet.sh       # runs deposit -> mint -> redeem on-chain
```

Live testnet contract IDs (see [`deploy/testnet.json`](deploy/testnet.json)):

| Contract | ID |
|---|---|
| Vault | `CCKMAPT5JPPO4QIAYNU225OBWFJISZL72PSKB4I2B6Y227JUJ7RQ5JCO` |
| baUSD token | `CBH5TF432BVE7GPZMTVQGM57E7357S2MZRHPFAI5FUTU7KKXS7RXRU4P` |
| Mock USDC (SAC) | `CAJBB6LISKXJN5ON2CCFTGPGEUMK7RNH3SGWIXXHD6Z4NFWZTWKINW2U` |

**USDC on testnet:** we deploy our own mock USDC as a classic asset wrapped in a Stellar
Asset Contract (issuer = deployer), so accounts establish a trustline exactly as they would
for real USDC. On mainnet this is swapped for the real Circle USDC SAC.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
