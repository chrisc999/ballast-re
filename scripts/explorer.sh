# Shared by the testnet walkthroughs: run stellar-cli quietly, but keep a stellar.expert
# link for every transaction actually submitted, so each step can show where to verify it.
# Sourced, not executed. Expects NETWORK to be set.

EXPLORER="https://stellar.expert/explorer/${NETWORK:-testnet}"
_TXLOG="$(mktemp)"
trap 'rm -f "$_TXLOG"' EXIT

# txrun LABEL CMD...  Run CMD with stdout passed through and stderr captured; if the CLI
# reports a submitted transaction, remember its explorer link under LABEL. Read-only
# calls are only simulated, so they leave no link.
txrun() {
  local label="$1"; shift
  local err rc=0
  err="$(mktemp)"
  "$@" 2>"$err" || rc=$?
  { grep -oE 'https://stellar\.expert/explorer/[a-z]+/tx/[0-9a-f]{64}' "$err" || true; } \
    | head -1 | while read -r url; do printf '%s %s\n' "$label" "$url" >> "$_TXLOG"; done
  rm -f "$err"
  return "$rc"
}

call()  { txrun "$3" stellar contract invoke --id "$1" --source "$2" --network "$NETWORK" -- "${@:3}"; }
silent(){ call "$@" >/dev/null; }

# Print the links collected since the last flush, one per transaction.
txlinks() {
  local label url
  while read -r label url; do
    printf '   ↳ %-20s %s\n' "$label" "$url"
  done < "$_TXLOG"
  : > "$_TXLOG"
}
