#!/usr/bin/env bash
# Demo: attested NAV, published price, compliance gating and the strategy adapter,
# on Stellar testnet. Clean output suitable for screen-recording.
# Run AFTER scripts/deploy_testnet.sh — against a FRESH deploy, so share price starts at
# exactly 1.0 and the printed figures are pristine. Re-running against a vault that has
# already been attested still works, it just starts mid-story: step 7 trims any sleeve
# surplus left by earlier runs so the partial fill still shows.
#
# The run temporarily turns the allowlist on and moves the treasury role to a throwaway
# key. Both are restored on exit, including when a step fails part-way.
#
#   ./scripts/demo_nav_testnet.sh
#
# Complements demo_testnet.sh, which covers the plain deposit -> mint -> redeem cycle.
# Each run uses a fresh LP account so the numbers are always pristine.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
J="$ROOT/deploy/testnet.json"
NETWORK="${NETWORK:-testnet}"
DEPLOYER="${DEPLOYER:-ballast-deployer}"
USER_ID="${USER_ID:-ballast-lp-$(date +%s)}"

FUND_USDC="${FUND_USDC:-2000000000}"  # 200 USDC (7 decimals)
DEPOSIT="${DEPOSIT:-1000000000}"      # 100 USDC

field() { python3 -c "import json;print(json.load(open('$J'))['$1'])"; }
VAULT="$(field vault)"; TOKEN="$(field token)"; USDC="$(field usdc_sac)"
ORACLE="$(field oracle)"; STRATEGY="$(field strategy)"; DEP_PK="$(field deployer)"
CADENCE="$(field nav_cadence_secs)"

# Quiet stellar-cli wrappers that keep an explorer link per transaction (call, silent,
# txrun, txlinks).
source "$ROOT/scripts/explorer.sh"
# For calls that may legitimately no-op on a re-run (e.g. `record` when the vault has not
# re-attested since last time). A failure here is expected, not a fault.
maybe() { silent "$@" || true; }
# Returns the contract error code (e.g. 26) when a call is expected to fail. The invoke is
# SUPPOSED to fail here, so its non-zero exit must not trip `set -e`/`pipefail`.
errcode(){
  local out
  out="$(stellar contract invoke --id "$1" --source "$2" --network "$NETWORK" -- "${@:3}" 2>&1 || true)"
  printf '%s' "$out" | grep -oE '#[0-9]+' | head -1 | tr -d '#' || true
}
usd()   { call "$USDC"  "$DEPLOYER" balance --id "$1" | tr -d '"'; }
bau()   { call "$TOKEN" "$DEPLOYER" balance --account "$1" | tr -d '"'; }
vaultv(){ call "$VAULT" "$DEPLOYER" "$1" | tr -d '"'; }
h()     { python3 -c "import sys;print(f'{int(sys.argv[1])/1e7:,.2f}')" "$1"; }
p()     { python3 -c "import sys;print(f'{int(sys.argv[1])/1e7:,.7f}')" "$1"; }
sa()    { printf '%s…%s' "${1:0:6}" "${1: -4}"; }
line()  { printf '   %-30s %s\n' "$1" "$2"; }
hdr()   { txlinks; printf '\n%s\n' "$1"; }

# Put the vault back the way we found it - allowlist switch and treasury role - on any
# exit, so an aborted run cannot break demo_testnet.sh or the next rehearsal.
ORIG_ALLOWLIST="$(vaultv allowlist_enabled)"
ORIG_TREASURY="$(call "$VAULT" "$DEPLOYER" get_config | python3 -c "import json,sys;print(json.load(sys.stdin)['treasury'])")"
cleanup() {
  local rc=$?
  silent "$VAULT" "$DEPLOYER" set_allowlist_enabled --enabled "$ORIG_ALLOWLIST" || true
  silent "$VAULT" "$DEPLOYER" set_treasury --new_treasury "$ORIG_TREASURY" || true
  rm -f "$_TXLOG"
  if [ "$rc" -ne 0 ]; then
    printf '\n   ✗ demo aborted (exit %s); vault settings restored. Re-run to retry.\n\n' "$rc"
  fi
}
trap cleanup EXIT

printf '\n════════════════════════════════════════════════════════════\n'
printf '   baUSD  ·  attested NAV, published price, compliance\n'
printf '════════════════════════════════════════════════════════════\n'

hdr "[1/8]  Fresh LP and treasury accounts, funded with test USDC"
TREASURY_ID="${TREASURY_ID:-ballast-treasury-$(date +%s)}"
stellar keys generate "$TREASURY_ID" --network "$NETWORK" --fund >/dev/null 2>&1 || true
TREASURY_PK="$(stellar keys address "$TREASURY_ID")"
txrun change_trust stellar tx new change-trust --source-account "$TREASURY_ID" --network "$NETWORK" --line "USDC:$DEP_PK" >/dev/null
silent "$VAULT" "$DEPLOYER" set_treasury --new_treasury "$TREASURY_PK"
stellar keys generate "$USER_ID" --network "$NETWORK" --fund >/dev/null 2>&1 || \
  stellar keys fund "$USER_ID" --network "$NETWORK" >/dev/null 2>&1 || true
USER_PK="$(stellar keys address "$USER_ID")"
txrun change_trust stellar tx new change-trust --source-account "$USER_ID" --network "$NETWORK" --line "USDC:$DEP_PK" >/dev/null
silent "$USDC" "$DEPLOYER" mint --to "$USER_PK" --amount "$FUND_USDC"
line "LP account" "$(sa "$USER_PK")"
line "treasury account" "$(sa "$TREASURY_PK")"
line "LP USDC" "$(h "$(usd "$USER_PK")")"

hdr "[2/8]  Compliance gate ON — an unlisted LP cannot subscribe"
silent "$VAULT" "$DEPLOYER" set_allowlist_enabled --enabled true
CODE="$(errcode "$VAULT" "$USER_ID" subscribe --from "$USER_PK" --amount "$DEPOSIT")"
line "allowlist enabled" "$(vaultv allowlist_enabled)"
line "subscribe by unlisted LP" "rejected — contract error #${CODE:-?} (NotAllowed)"

hdr "[3/8]  Compliance authority allowlists the LP, who then subscribes"
silent "$VAULT" "$DEPLOYER" set_allowed --who "$USER_PK" --allowed true
SHARES="$(call "$VAULT" "$USER_ID" subscribe --from "$USER_PK" --amount "$DEPOSIT" | tr -d '"')"
line "LP allowlisted" "$(call "$VAULT" "$DEPLOYER" is_allowed --who "$USER_PK")"
line "deposited" "$(h "$DEPOSIT") USDC"
line "baUSD minted" "$(h "$SHARES")"
line "share price" "$(p "$(vaultv share_price)")"

hdr "[4/8]  Price published through the SEP-40 feed"
maybe "$ORACLE" "$DEPLOYER" record
line "feed lastprice" "$(call "$ORACLE" "$DEPLOYER" lastprice --asset "{\"Stellar\":\"$TOKEN\"}")"
line "feed decimals / resolution" "$(call "$ORACLE" "$DEPLOYER" decimals) / $(call "$ORACLE" "$DEPLOYER" resolution)s"

hdr "[5/8]  Guards refuse a bad attestation before a good one is accepted"
sleep "$((CADENCE + 5))"
OVER=$(python3 -c "print($(vaultv total_assets) + $(vaultv max_nav_delta) * 3)")
CODE="$(errcode "$VAULT" "$DEPLOYER" update_nav --new_total_assets "$OVER" \
  --proof_ref 0808080808080808080808080808080808080808080808080808080808080808 --signers "[\"$DEP_PK\"]")"
line "attestable budget" "$(h "$(vaultv max_nav_delta)") USDC  (2% of the attested baseline)"
line "move of $(h $(( OVER - $(vaultv total_assets) ))) USDC" "rejected — contract error #${CODE:-?}"
CODE="$(errcode "$VAULT" "$DEPLOYER" update_nav --new_total_assets "$(vaultv total_assets)" \
  --proof_ref 0909090909090909090909090909090909090909090909090909090909090909 --signers "[\"$USER_PK\"]")"
line "attestation by a stranger" "rejected — contract error #${CODE:-?}"

hdr "[6/8]  A valid attestation from the quorum, and the price republished"
NEW_NAV=$(python3 -c "print($(vaultv total_assets) + $(vaultv max_nav_delta) // 2)")
silent "$VAULT" "$DEPLOYER" update_nav \
  --new_total_assets "$NEW_NAV" \
  --proof_ref 0707070707070707070707070707070707070707070707070707070707070707 \
  --signers "[\"$DEP_PK\"]"
line "attested NAV" "$(h "$(vaultv total_assets)") USDC"
line "share price" "$(p "$(vaultv share_price)")   ← appreciated"
line "LP position now worth" "$(h "$(call "$VAULT" "$DEPLOYER" convert_to_assets --shares "$SHARES" | tr -d '"')") USDC"
maybe "$ORACLE" "$DEPLOYER" record
line "feed lastprice" "$(call "$ORACLE" "$DEPLOYER" lastprice --asset "{\"Stellar\":\"$TOKEN\"}")"

hdr "[7/8]  Redemption partially fills — the gain is not in the sleeve yet"
# Earlier runs (or the DeFindex walkthrough) can leave extra USDC in the sleeve. Send the
# surplus out to treaties first so the sleeve holds only this LP's deposit, as it does on
# a fresh deploy. deploy_capital is NAV-neutral, so the share price is unaffected.
SURPLUS=$(( $(vaultv sleeve_balance) - DEPOSIT ))
if [ "$SURPLUS" -gt 0 ]; then
  silent "$VAULT" "$TREASURY_ID" deploy_capital --amount "$SURPLUS"
fi
line "vault sleeve (on-chain USDC)" "$(h "$(vaultv sleeve_balance)")"
line "owed at claim-time NAV" "$(h "$(call "$VAULT" "$DEPLOYER" convert_to_assets --shares "$SHARES" | tr -d '"')")"
silent "$VAULT" "$USER_ID" request_redemption --from "$USER_PK" --shares "$SHARES"
PAID1="$(call "$VAULT" "$USER_ID" claim_redemption --from "$USER_PK" | tr -d '"')"
line "paid now" "$(h "$PAID1")   ← limited by the sleeve"
line "still queued" "$(h "$(call "$VAULT" "$DEPLOYER" get_redemption --who "$USER_PK" | python3 -c "import sys,json;r=json.load(sys.stdin);print(r['shares'] if r else 0)" | tr -d '"')") baUSD"

hdr "[8/8]  Treasury returns capital from treaties; the claim completes"
silent "$USDC" "$DEPLOYER" mint --to "$TREASURY_PK" --amount 100000000
silent "$VAULT" "$TREASURY_ID" fund_sleeve --amount 100000000
line "sleeve refilled to" "$(h "$(vaultv sleeve_balance)")"
PAID2="$(call "$VAULT" "$USER_ID" claim_redemption --from "$USER_PK" | tr -d '"')"
line "paid on completion" "$(h "$PAID2")"
line "total returned to LP" "$(h "$((PAID1 + PAID2))")   ← more than the $(h "$DEPOSIT") deposited"
line "redemption fully settled" "$(call "$VAULT" "$DEPLOYER" get_redemption --who "$USER_PK")"
txlinks

printf '\nVerify on stellar.expert\n'
line "LP account (all its txs)" "$EXPLORER/account/$USER_PK"
line "vault contract" "$EXPLORER/contract/$VAULT"
line "SEP-40 price feed" "$EXPLORER/contract/$ORACLE"
line "baUSD token" "$EXPLORER/contract/$TOKEN"

printf '\n════════════════════════════════════════════════════════════\n'
printf '   NAV attested on-chain · price published via SEP-40\n'
printf '   compliance enforced · appreciation paid out\n'
printf '════════════════════════════════════════════════════════════\n\n'
