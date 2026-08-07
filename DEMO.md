# Demo — deposit → mint → redeem on testnet

A **recorded run of the full deposit → mint → redeem cycle on Stellar testnet**, under 3
minutes. Everything is scripted so the recording is a single command.

## What to record

1. Open a terminal (screen recording on).
2. Show the live contracts once (optional): open the vault on
   [stellar.expert testnet](https://stellar.expert/explorer/testnet/contract/CCKMAPT5JPPO4QIAYNU225OBWFJISZL72PSKB4I2B6Y227JUJ7RQ5JCO).
3. Run:

   ```bash
   ./scripts/demo_testnet.sh
   ```

4. Narrate as each step prints:
   - **Mint** — the demo LP receives 100 mock USDC (real USDC is a classic asset; they take
     a trustline first, just like mainnet).
   - **Subscribe** — LP deposits 50 USDC; the vault mints 50 baUSD (first deposit is 1:1).
   - **Balances** — LP now holds 50 baUSD and 50 USDC; vault `total_shares`/`total_assets`
     read 50.
   - **Request redemption** — LP's 50 baUSD are escrowed in the vault (wallet baUSD → 0).
   - **Claim** — notice period is 0 on testnet, so the LP claims immediately: escrowed baUSD
     is **burned**, 50 USDC is returned. baUSD `total_supply` → 0.

A verified transcript from a real run is in [`deploy/demo-transcript.txt`](deploy/demo-transcript.txt),
and a recorded animation of an actual on-chain run is below:

![baUSD testnet demo](media/demo.gif)

(Source recording: [`media/demo.cast`](media/demo.cast) — replayable with `asciinema play`.)

## Reproduce from scratch

```bash
./scripts/deploy_testnet.sh   # fresh vault + token + mock USDC, wired, addresses -> deploy/testnet.json
./scripts/demo_testnet.sh     # the cycle above
```

## Live IDs (testnet)

| Contract | ID | Explorer |
|---|---|---|
| Vault | `CCKMAPT5JPPO4QIAYNU225OBWFJISZL72PSKB4I2B6Y227JUJ7RQ5JCO` | [view](https://stellar.expert/explorer/testnet/contract/CCKMAPT5JPPO4QIAYNU225OBWFJISZL72PSKB4I2B6Y227JUJ7RQ5JCO) |
| baUSD token | `CBH5TF432BVE7GPZMTVQGM57E7357S2MZRHPFAI5FUTU7KKXS7RXRU4P` | [view](https://stellar.expert/explorer/testnet/contract/CBH5TF432BVE7GPZMTVQGM57E7357S2MZRHPFAI5FUTU7KKXS7RXRU4P) |
| Mock USDC | `CAJBB6LISKXJN5ON2CCFTGPGEUMK7RNH3SGWIXXHD6Z4NFWZTWKINW2U` | [view](https://stellar.expert/explorer/testnet/contract/CAJBB6LISKXJN5ON2CCFTGPGEUMK7RNH3SGWIXXHD6Z4NFWZTWKINW2U) |
