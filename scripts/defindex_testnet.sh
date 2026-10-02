#!/usr/bin/env bash
# Create a real DeFindex vault on testnet that allocates to baUSD through our strategy
# adapter, then run a deposit and withdrawal through it. Clean output suitable for
# screen-recording.
#
# Run AFTER scripts/deploy_testnet.sh. Re-runnable: the DeFindex vault is created once and
# recorded in deploy/testnet.json; later runs reuse it (FORCE_NEW=1 to create another).
#
#   ./scripts/defindex_testnet.sh
#
# The factory and router addresses are the published testnet deployments:
#   factory — defindex-io/stellar-contracts  public/testnet.contracts.json
#   router  — soroswap/core       public/testnet.contracts.json
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
J="$ROOT/deploy/testnet.json"
NETWORK="${NETWORK:-testnet}"
DEPLOYER="${DEPLOYER:-ballast-deployer}"
USER_ID="${USER_ID:-ballast-df-lp-$(date +%s)}"

FACTORY="${FACTORY:-CDSCWE4GLNBYYTES2OCYDFQA2LLY4RBIAX6ZI32VSUXD7GO6HRPO4A32}"
ROUTER="${ROUTER:-CCJUD55AG6W5HAI5LRVNKAE5WDP5XGZBUDS5WNTIVDU7O264UZZE7BRD}"
VAULT_FEE_BPS="${VAULT_FEE_BPS:-100}"        # DeFindex vault fee, basis points
FUND_USDC="${FUND_USDC:-2000000000}"         # 200 USDC (7 decimals)
DEPOSIT="${DEPOSIT:-1000000000}"             # 100 USDC
WITHDRAW="${WITHDRAW:-400000000}"            # 40 USDC back out through the atomic path

field() { python3 -c "import json;print(json.load(open('$J')).get('$1',''))"; }
VAULT="$(field vault)"; TOKEN="$(field token)"; USDC="$(field usdc_sac)"
STRATEGY="$(field strategy)"; DEP_PK="$(field deployer)"
DFVAULT="$(field defindex_vault)"

# Quiet stellar-cli wrappers that keep an explorer link per transaction (call, silent,
# txrun, txlinks).
source "$ROOT/scripts/explorer.sh"
usd()   { call "$USDC"  "$DEPLOYER" balance --id "$1" | tr -d '"'; }
bau()   { call "$TOKEN" "$DEPLOYER" balance --account "$1" | tr -d '"'; }
h()     { python3 -c "import sys;print(f'{int(sys.argv[1])/1e7:,.2f}')" "$1"; }
sa()    { printf '%s…%s' "${1:0:6}" "${1: -4}"; }
line()  { printf '   %-34s %s\n' "$1" "$2"; }
hdr()   { txlinks; printf '\n%s\n' "$1"; }

printf '\n════════════════════════════════════════════════════════════\n'
printf '   baUSD  ·  selectable as a DeFindex strategy\n'
printf '════════════════════════════════════════════════════════════\n'

hdr "[1/6]  The adapter is allowlisted at the baUSD vault (subscriber of record)"
silent "$VAULT" "$DEPLOYER" set_allowed --who "$STRATEGY" --allowed true
line "strategy adapter" "$(sa "$STRATEGY")"
line "allowlisted" "$(call "$VAULT" "$DEPLOYER" is_allowed --who "$STRATEGY")"

hdr "[2/6]  A DeFindex vault is created from their public testnet factory"
if [ -z "$DFVAULT" ] || [ "${FORCE_NEW:-0}" = "1" ]; then
  DFVAULT="$(call "$FACTORY" "$DEPLOYER" create_defindex_vault \
    --roles "{\"0\":\"$DEP_PK\",\"1\":\"$DEP_PK\",\"2\":\"$DEP_PK\",\"3\":\"$DEP_PK\"}" \
    --vault_fee "$VAULT_FEE_BPS" \
    --assets "[{\"address\":\"$USDC\",\"strategies\":[{\"address\":\"$STRATEGY\",\"name\":\"baUSD\",\"paused\":false}]}]" \
    --soroswap_router "$ROUTER" \
    --name_symbol "{\"name\":\"Ballast baUSD Vault\",\"symbol\":\"dfBAUSD\"}" \
    --upgradable false | tr -d '"')"
  python3 - "$J" "$FACTORY" "$DFVAULT" <<'EOF'
import json, sys
p, factory, dfvault = sys.argv[1:]
d = json.load(open(p))
d["defindex_factory"] = factory
d["defindex_vault"] = dfvault
json.dump(d, open(p, "w"), indent=2)
open(p, "a").write("\n")
EOF
  line "created" "$(sa "$DFVAULT")  (recorded in deploy/testnet.json)"
else
  line "reusing" "$(sa "$DFVAULT")  (FORCE_NEW=1 for a fresh one)"
fi
line "asset" "test USDC  ·  strategy: baUSD"

hdr "[3/6]  A depositor is funded with test USDC"
stellar keys generate "$USER_ID" --network "$NETWORK" --fund >/dev/null 2>&1 || true
USER_PK="$(stellar keys address "$USER_ID")"
txrun change_trust stellar tx new change-trust --source-account "$USER_ID" --network "$NETWORK" --line "USDC:$DEP_PK" >/dev/null
silent "$USDC" "$DEPLOYER" mint --to "$USER_PK" --amount "$FUND_USDC"
line "depositor" "$(sa "$USER_PK")"
line "USDC balance" "$(h "$(usd "$USER_PK")")"

hdr "[4/6]  Deposit into the DeFindex vault, then the manager allocates to baUSD"
call "$DFVAULT" "$USER_ID" deposit \
  --amounts_desired "[\"$DEPOSIT\"]" --amounts_min "[\"$DEPOSIT\"]" \
  --from "$USER_PK" --invest true >/dev/null
# `invest: true` allocates proportionally to EXISTING allocations, so the very first
# deposit lands idle; the rebalance manager invests whatever idles.
IDLE="$(call "$DFVAULT" "$DEPLOYER" fetch_total_managed_funds \
  | python3 -c "import json,sys;print(json.load(sys.stdin)[0]['idle_amount'])")"
if [ "$IDLE" -gt 0 ]; then
  call "$DFVAULT" "$DEPLOYER" rebalance --caller "$DEP_PK" \
    --instructions "[{\"Invest\":[\"$STRATEGY\",\"$IDLE\"]}]" >/dev/null
fi
INVESTED="$(call "$DFVAULT" "$DEPLOYER" fetch_total_managed_funds \
  | python3 -c "import json,sys;print(json.load(sys.stdin)[0]['invested_amount'])")"
line "deposited" "$(h "$DEPOSIT") USDC"
line "df-shares held" "$(h "$(call "$DFVAULT" "$DEPLOYER" balance --id "$USER_PK" | tr -d '"')")"
line "invested in baUSD strategy" "$(h "$INVESTED") USDC"

hdr "[5/6]  The position is real baUSD at our vault, held by the adapter"
line "adapter position (strategy)" "$(h "$(call "$STRATEGY" "$DEPLOYER" balance --from "$DFVAULT" | tr -d '"')") USDC"
line "adapter baUSD" "$(h "$(bau "$STRATEGY")")"
line "baUSD vault total_assets" "$(h "$(call "$VAULT" "$DEPLOYER" total_assets | tr -d '"')")"

hdr "[6/6]  Withdrawal settles atomically through request + claim"
call "$DFVAULT" "$USER_ID" withdraw \
  --withdraw_shares "$WITHDRAW" --min_amounts_out "[\"0\"]" --from "$USER_PK" >/dev/null
line "withdrawn" "$(h "$WITHDRAW") df-shares"
line "depositor USDC" "$(h "$(usd "$USER_PK")")"
line "adapter position (strategy)" "$(h "$(call "$STRATEGY" "$DEPLOYER" balance --from "$DFVAULT" | tr -d '"')") USDC"

txlinks

printf '\nVerify on stellar.expert\n'
line "depositor account (all its txs)" "$EXPLORER/account/$USER_PK"
line "DeFindex vault" "$EXPLORER/contract/$DFVAULT"
line "baUSD strategy adapter" "$EXPLORER/contract/$STRATEGY"
line "baUSD vault" "$EXPLORER/contract/$VAULT"
printf '\nbaUSD is live as a DeFindex strategy on testnet: %s\n\n' "$DFVAULT"
