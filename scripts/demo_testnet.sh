#!/usr/bin/env bash
# Demo: deposit -> mint -> redeem on Stellar testnet, with clean output
# suitable for screen-recording. Run AFTER scripts/deploy_testnet.sh.
#
#   ./scripts/demo_testnet.sh
#
# Each run uses a fresh LP account so the numbers are always pristine.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
J="$ROOT/deploy/testnet.json"
NETWORK="${NETWORK:-testnet}"
DEPLOYER="${DEPLOYER:-ballast-deployer}"
# Fresh LP identity each run (override with USER_ID=... to reuse one).
USER_ID="${USER_ID:-ballast-lp-$(date +%s)}"

FUND_USDC="${FUND_USDC:-1000000000}"  # 100 USDC (7 decimals)
DEPOSIT="${DEPOSIT:-500000000}"       #  50 USDC

field() { python3 -c "import json;print(json.load(open('$J'))['$1'])"; }
VAULT="$(field vault)"; TOKEN="$(field token)"; USDC="$(field usdc_sac)"; DEP_PK="$(field deployer)"

# stellar-cli diagnostics go to stderr; suppress them so the recording is clean.
call()  { stellar contract invoke --id "$1" --source "$2" --network "$NETWORK" -- "${@:3}" 2>/dev/null; }
silent(){ stellar contract invoke --id "$1" --source "$2" --network "$NETWORK" -- "${@:3}" >/dev/null 2>&1; }
usd()   { call "$USDC"  "$DEPLOYER" balance --id "$1" | tr -d '"'; }
bau()   { call "$TOKEN" "$DEPLOYER" balance --account "$1" | tr -d '"'; }
vaultv(){ call "$VAULT" "$DEPLOYER" "$1" | tr -d '"'; }
h()     { python3 -c "import sys;print(f'{int(sys.argv[1])/1e7:,.2f}')" "$1"; }        # stroops -> units
sa()    { printf '%s…%s' "${1:0:6}" "${1: -4}"; }
line()  { printf '   %-26s %s\n' "$1" "$2"; }

printf '\n════════════════════════════════════════════════════════════\n'
printf '   baUSD  ·  deposit → mint → redeem  ·  Stellar testnet\n'
printf '════════════════════════════════════════════════════════════\n'
line "Vault" "$(sa "$VAULT")"
line "baUSD token" "$(sa "$TOKEN")"
line "USDC (mock)" "$(sa "$USDC")"

printf '\n[1/5]  New LP account, funded on testnet\n'
stellar keys generate "$USER_ID" --network "$NETWORK" --fund >/dev/null 2>&1 || \
  stellar keys fund "$USER_ID" --network "$NETWORK" >/dev/null 2>&1 || true
USER_PK="$(stellar keys address "$USER_ID")"
# Real USDC is a classic asset; the LP takes a trustline just like on mainnet.
stellar tx new change-trust --source-account "$USER_ID" --network "$NETWORK" --line "USDC:$DEP_PK" >/dev/null 2>&1
line "LP account" "$(sa "$USER_PK")"

printf '\n[2/5]  LP receives %s USDC (mock issuer; real USDC on mainnet)\n' "$(h "$FUND_USDC")"
silent "$USDC" "$DEPLOYER" mint --to "$USER_PK" --amount "$FUND_USDC"
line "LP USDC" "$(h "$(usd "$USER_PK")")"

printf '\n[3/5]  LP subscribes %s USDC  →  vault mints baUSD\n' "$(h "$DEPOSIT")"
SHARES="$(call "$VAULT" "$USER_ID" subscribe --from "$USER_PK" --amount "$DEPOSIT" | tr -d '"')"
TA="$(vaultv total_assets)"; TS="$(vaultv total_shares)"
PRICE="$(python3 -c "import sys;ts=int(sys.argv[2]);print(f'{int(sys.argv[1])/ts:.4f}' if ts else '-')" "$TA" "$TS")"
line "baUSD minted" "$(h "$SHARES")"
line "LP baUSD balance" "$(h "$(bau "$USER_PK")")"
line "LP USDC balance" "$(h "$(usd "$USER_PK")")"
line "vault NAV (assets)" "$(h "$TA")"
line "vault shares" "$(h "$TS")"
line "share price" "$PRICE"

printf '\n[4/5]  LP requests redemption of %s baUSD (escrowed in vault)\n' "$(h "$SHARES")"
silent "$VAULT" "$USER_ID" request_redemption --from "$USER_PK" --shares "$SHARES"
line "LP baUSD balance" "$(h "$(bau "$USER_PK")")"

printf '\n[5/5]  LP claims  →  baUSD burned, USDC returned (notice = 0)\n'
silent "$VAULT" "$USER_ID" claim_redemption --from "$USER_PK"
line "LP baUSD balance" "$(h "$(bau "$USER_PK")")"
line "LP USDC balance" "$(h "$(usd "$USER_PK")")"
line "baUSD total supply" "$(h "$(call "$TOKEN" "$DEPLOYER" total_supply | tr -d '"')")"

printf '\n════════════════════════════════════════════════════════════\n'
printf '   ✅  Full cycle complete on Stellar testnet\n'
printf '   explorer: stellar.expert/explorer/testnet/contract/%s\n' "$VAULT"
printf '════════════════════════════════════════════════════════════\n\n'
