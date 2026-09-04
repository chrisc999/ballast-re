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
  `share_price` published for external price feeds. Liquidity: `fund_sleeve` /
  `deploy_capital` (NAV-neutral treasury moves), partial-fill claims that queue the
  remainder rather than failing, and `set_redemptions_suspended`. Compliance: an allowlist
  gate on subscribe/redeem, managed by the compliance authority and switched on by
  governance. Planned: per-period redemption caps.
- **Escrow is always reversible** — `request_redemption` moves baUSD into the vault, so
  `cancel_redemption` is gated on nothing but the holder's own signature (not pause, not
  compliance). A request that cannot be claimed can always be undone.
- **Depeg guard** — NAV is attested in USD terms while subscriptions settle in USDC. While
  USDC trades at $1 those are the same thing; if it depegs they are not, and the gap is
  exploitable on deposit. An optional SEP-40 feed (Reflector's, or any other) gates new
  deposits when the settlement asset drifts more than 2% from parity. Exits are never gated.
- **Compliance is address-only** — KYC/AML happens off-chain; on approval the compliance
  authority writes the address to the on-chain allowlist. The contract never sees identity
  data. Deciding who is on the list and switching the gate off are deliberately different
  authorities.
- **Price feed** — a SEP-40 oracle contract publishing the vault's attested share price.
  baUSD does not trade, so its price cannot be *discovered* by sampling a market; it is
  *published* from attested NAV. SEP-40 is a pure interface standard, so anything that can
  already read a SEP-40 feed reads baUSD unchanged. The vault stays the single source of
  truth — the feed only ever copies `share_price()`, and `record()` is permissionless.
- **DeFindex strategy adapter** — exposes the vault as an allocatable DeFindex strategy.
  DeFindex's `withdraw` is synchronous and baUSD is not built to be: the adapter settles a
  withdrawal only when the vault can honour it atomically (notice elapsed, sleeve covering
  the full amount) and otherwise fails, which rolls the redemption request back with it so
  no escrow is stranded. An illiquid strategy that reports its limits truthfully is safer
  than one that pretends.
- **Roles** (separation of duties) — governance multisig (admin/upgrade), guardian
  (pause), an m-of-n attestation quorum (NAV), compliance authority (allowlist), treasury
  ops (sleeve). Designed as swappable addresses so mainnet multisig drops in later.
- **The sleeve is not the NAV** — most capital sits in reinsurance treaties, so the vault's
  on-chain USDC balance (`sleeve_balance`) is deliberately much smaller than `total_assets`.
  Moving capital between the two is NAV-neutral; only `update_nav` moves NAV.
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

## Web console

A React + Freighter console for the testnet deployment lives in [`app/`](app/):
connect a wallet, add the USDC trustline, deposit, request/claim/cancel redemptions,
and watch share price, NAV, and the sleeve live. Contract addresses are imported
straight from `deploy/testnet.json`, so the app can never drift from what is deployed.

```bash
cd app && npm install && npm run dev
```

Requires Node 18+. Freighter must be set to Testnet.

## Testnet deployment

One command deploys mock USDC + vault + baUSD token + price feed + strategy adapter and
wires them together:

```bash
./scripts/deploy_testnet.sh     # writes deploy/testnet.json
./scripts/demo_testnet.sh       # deposit -> mint -> redeem on-chain
./scripts/demo_nav_testnet.sh   # attested NAV, published price, compliance, sleeve
./scripts/defindex_testnet.sh   # live DeFindex vault allocating to baUSD
```

Always a **fresh** deploy rather than an in-place wasm upgrade: the vault's `Config` shape
and storage key set have changed across versions, so swapping the wasm under a live instance
would leave the stored config undeserializable. New addresses per deploy is correct here.

Live testnet contract IDs (see [`deploy/testnet.json`](deploy/testnet.json)):

| Contract | ID |
|---|---|
| Vault | `CAYRNUTZF5Y5AURQEUIIOTWMZOYDNSR3XS3N6FLJZOXHNUTSKDAYBTEY` |
| baUSD token | `CAKIZ24RUL4EAJNEBX64HIZHW3O27KVAEYM47MHYXMAQECLX4TTHRQYM` |
| SEP-40 price feed | `CBDKXYQO46VJUVGSFI2BHA5MZO5C5E6D6GZLZ3Z26SROFDG4RJSLATYX` |
| DeFindex strategy | `CDYS2RH7KTWRSL7FN3WXXBYDL7P2N2ZEPMZY65SWTJ75ZU34RAPL7Y4E` |
| DeFindex vault (holds the strategy) | `CCYOQRFSM3EJLBSFWWEBTF5UGLFHVTZ7FUNVPDACG7PPQU6Z2XC3ECMU` |
| Mock USDC (SAC) | `CAJBB6LISKXJN5ON2CCFTGPGEUMK7RNH3SGWIXXHD6Z4NFWZTWKINW2U` |

**NAV cadence on testnet** is set to 60s rather than the 20h default, so the
attestation flow is demonstrable immediately after deploy. Mainnet governance sets this to
the real attestation cadence.

**The depeg guard is unconfigured on testnet** — the mock USDC below is ours and no public
oracle carries it. On mainnet, `set_price_oracle` points it at a real SEP-40 feed.

**USDC on testnet:** we deploy our own mock USDC as a classic asset wrapped in a Stellar
Asset Contract (issuer = deployer), so accounts establish a trustline exactly as they would
for real USDC. On mainnet this is swapped for the real Circle USDC SAC.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
