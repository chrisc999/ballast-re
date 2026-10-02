#!/usr/bin/env bash
# Reproducible one-command testnet deploy for baUSD.
#
#   ./scripts/deploy_testnet.sh
#
# Deploys: mock USDC (a classic asset wrapped as a Stellar Asset Contract), the vault, the
# baUSD token (owned by the vault), the SEP-40 price feed and the DeFindex strategy adapter,
# then wires them together. Writes the resulting contract IDs to deploy/testnet.json.
#
# Always a FRESH deploy, never an in-place wasm upgrade. The vault's Config struct and its
# storage key set have both changed since earlier deployments, so upgrading the wasm under a
# live instance would leave the stored Config undeserializable and the new keys absent. New
# addresses each run is the correct behaviour here, not a limitation.
#
# The settlement-asset depeg guard is deliberately left UNCONFIGURED on testnet: the mock
# USDC below is ours and no public oracle carries it. On mainnet, call set_price_oracle with
# a real feed.
#
# USDC on testnet: we deploy our OWN mock USDC (issuer = the deployer) so the demo can mint
# test USDC freely. On mainnet this is swapped for the real Circle USDC SAC.
#
# Idempotency: stellar keys are reused if they already exist; contracts are always fresh.
set -euo pipefail

NETWORK="${NETWORK:-testnet}"
DEPLOYER="${DEPLOYER:-ballast-deployer}"
OUT_DIR="$(cd "$(dirname "$0")/.." && pwd)/deploy"
WASM_DIR="$(cd "$(dirname "$0")/.." && pwd)/target/wasm32v1-none/release"
TOKEN_NAME="${TOKEN_NAME:-Ballast USD}"
TOKEN_SYMBOL="${TOKEN_SYMBOL:-baUSD}"
NOTICE_PERIOD="${NOTICE_PERIOD:-0}"
# Routine NAV attestations are spaced 20h apart by default, which would make a fresh
# deployment un-attestable for most of a day. Testnet uses a short cadence so the NAV flow
# is demonstrable immediately; mainnet governance sets this to the real attestation cadence.
NAV_CADENCE="${NAV_CADENCE:-60}"

echo "==> Building contracts (wasm)"
stellar contract build >/dev/null

echo "==> Ensuring testnet network alias"
stellar network add "$NETWORK" \
  --rpc-url https://soroban-testnet.stellar.org \
  --network-passphrase "Test SDF Network ; September 2015" 2>/dev/null || true

echo "==> Ensuring funded deployer identity: $DEPLOYER"
if ! stellar keys address "$DEPLOYER" >/dev/null 2>&1; then
  stellar keys generate "$DEPLOYER" --network "$NETWORK" --fund
else
  stellar keys fund "$DEPLOYER" --network "$NETWORK" 2>/dev/null || true
fi
DEPLOYER_PK="$(stellar keys address "$DEPLOYER")"
echo "    deployer: $DEPLOYER_PK"

echo "==> Deploying mock USDC (issuer = deployer) as a Stellar Asset Contract"
# The SAC address is deterministic from the asset, so a second run just reuses it.
USDC_ID="$(stellar contract asset deploy \
  --asset "USDC:$DEPLOYER_PK" \
  --source "$DEPLOYER" --network "$NETWORK" 2>/dev/null \
  || stellar contract id asset --asset "USDC:$DEPLOYER_PK" --network "$NETWORK")"
echo "    USDC SAC: $USDC_ID"

echo "==> Deploying vault"
VAULT_ID="$(stellar contract deploy \
  --wasm "$WASM_DIR/ba_usd_vault.wasm" \
  --source "$DEPLOYER" --network "$NETWORK" \
  -- \
  --admin "$DEPLOYER_PK" \
  --guardian "$DEPLOYER_PK" \
  --attestation_authority "$DEPLOYER_PK" \
  --compliance_authority "$DEPLOYER_PK" \
  --treasury "$DEPLOYER_PK" \
  --usdc "$USDC_ID" \
  --notice_period "$NOTICE_PERIOD")"
echo "    vault: $VAULT_ID"

echo "==> Deploying baUSD token (owner = vault)"
TOKEN_ID="$(stellar contract deploy \
  --wasm "$WASM_DIR/ba_usd_token.wasm" \
  --source "$DEPLOYER" --network "$NETWORK" \
  -- \
  --owner "$VAULT_ID" \
  --name "$TOKEN_NAME" \
  --symbol "$TOKEN_SYMBOL")"
echo "    token: $TOKEN_ID"

echo "==> Binding token to vault (set_token)"
stellar contract invoke \
  --id "$VAULT_ID" --source "$DEPLOYER" --network "$NETWORK" \
  -- set_token --token "$TOKEN_ID" >/dev/null

echo "==> Setting testnet NAV cadence to ${NAV_CADENCE}s (mainnet uses the 20h default)"
stellar contract invoke \
  --id "$VAULT_ID" --source "$DEPLOYER" --network "$NETWORK" \
  -- set_nav_cadence --interval_secs "$NAV_CADENCE" >/dev/null

echo "==> Deploying SEP-40 price feed (publishes the vault's attested share price)"
ORACLE_ID="$(stellar contract deploy \
  --wasm "$WASM_DIR/ba_usd_oracle.wasm" \
  --source "$DEPLOYER" --network "$NETWORK" \
  -- \
  --vault "$VAULT_ID" \
  --quoted "$TOKEN_ID" \
  --base USD)"
echo "    oracle: $ORACLE_ID"

echo "==> Deploying DeFindex strategy adapter"
STRATEGY_ID="$(stellar contract deploy \
  --wasm "$WASM_DIR/ba_usd_strategy.wasm" \
  --source "$DEPLOYER" --network "$NETWORK" \
  -- \
  --asset "$USDC_ID" \
  --init_args "[{\"address\":\"$VAULT_ID\"}]")"
echo "    strategy: $STRATEGY_ID"

mkdir -p "$OUT_DIR"
cat > "$OUT_DIR/testnet.json" <<JSON
{
  "network": "$NETWORK",
  "deployer": "$DEPLOYER_PK",
  "usdc_sac": "$USDC_ID",
  "vault": "$VAULT_ID",
  "token": "$TOKEN_ID",
  "oracle": "$ORACLE_ID",
  "strategy": "$STRATEGY_ID",
  "notice_period": $NOTICE_PERIOD,
  "nav_cadence_secs": $NAV_CADENCE,
  "price_oracle_configured": false
}
JSON

echo
echo "==> Deployed. Addresses written to deploy/testnet.json:"
cat "$OUT_DIR/testnet.json"
