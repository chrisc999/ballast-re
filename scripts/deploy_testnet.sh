#!/usr/bin/env bash
# Reproducible one-command testnet deploy for baUSD.
#
#   ./scripts/deploy_testnet.sh
#
# Deploys: mock USDC (a classic asset wrapped as a Stellar Asset Contract), the vault, and
# the baUSD token (owned by the vault), then wires them together. Writes the resulting
# contract IDs to deploy/testnet.json.
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

mkdir -p "$OUT_DIR"
cat > "$OUT_DIR/testnet.json" <<JSON
{
  "network": "$NETWORK",
  "deployer": "$DEPLOYER_PK",
  "usdc_sac": "$USDC_ID",
  "vault": "$VAULT_ID",
  "token": "$TOKEN_ID",
  "notice_period": $NOTICE_PERIOD
}
JSON

echo
echo "==> Deployed. Addresses written to deploy/testnet.json:"
cat "$OUT_DIR/testnet.json"
