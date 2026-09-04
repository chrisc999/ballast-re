import { useCallback, useEffect, useMemo, useState } from "react";
import { getNetwork, isConnected, requestAccess } from "@stellar/freighter-api";
import {
  DECIMALS,
  SCALE,
  TOKEN_ID,
  USDC_CODE,
  USDC_ID,
  USDC_ISSUER,
  VAULT_ID,
} from "./config";
import {
  addUsdcTrustline,
  friendlyError,
  invoke,
  readView,
  scAddress,
  scI128,
} from "./stellar";
import "./App.css";

type Redemption = { shares: bigint; claimable_at: bigint } | null;

type VaultState = {
  sharePrice: bigint;
  totalAssets: bigint;
  totalShares: bigint;
  sleeve: bigint;
  navStale: boolean;
  suspended: boolean;
  allowlistOn: boolean;
};

type WalletState = {
  usdc: bigint | null; // null = no trustline
  baus: bigint;
  allowed: boolean;
  redemption: Redemption;
};

const fmt = (v: bigint) => {
  const neg = v < 0n;
  const a = neg ? -v : v;
  const whole = a / SCALE;
  const frac = (a % SCALE).toString().padStart(DECIMALS, "0").replace(/0+$/, "");
  return `${neg ? "-" : ""}${whole.toLocaleString("en-US")}${frac ? "." + frac : ""}`;
};

const parseAmount = (s: string): bigint | null => {
  const m = s.trim().match(/^(\d+)(?:\.(\d{1,7}))?$/);
  if (!m) return null;
  return BigInt(m[1]) * SCALE + BigInt((m[2] ?? "").padEnd(DECIMALS, "0") || "0");
};

const short = (a: string) => `${a.slice(0, 5)}…${a.slice(-4)}`;

export default function App() {
  const [address, setAddress] = useState<string | null>(null);
  const [wrongNetwork, setWrongNetwork] = useState(false);
  const [vault, setVault] = useState<VaultState | null>(null);
  const [wallet, setWallet] = useState<WalletState | null>(null);
  const [depositIn, setDepositIn] = useState("");
  const [redeemIn, setRedeemIn] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [note, setNote] = useState<{ kind: "ok" | "err"; text: string } | null>(null);

  const say = (kind: "ok" | "err", text: string) => setNote({ kind, text });

  const connect = useCallback(async () => {
    setNote(null);
    const here = await isConnected();
    if (!here.isConnected) {
      say("err", "Freighter isn't installed. Get it at freighter.app, then reload.");
      return;
    }
    const net = await getNetwork();
    if (net.network !== "TESTNET") {
      setWrongNetwork(true);
      say("err", "Freighter is on the wrong network — switch it to Testnet.");
      return;
    }
    setWrongNetwork(false);
    const access = await requestAccess();
    if (access.error || !access.address) {
      say("err", access.error?.message ?? "Freighter declined the connection.");
      return;
    }
    setAddress(access.address);
  }, []);

  const refresh = useCallback(async () => {
    // Views are simulations; any funded account can be the simulation source.
    const source = address ?? USDC_ISSUER;
    const v = async <T,>(id: string, method: string, args: Parameters<typeof readView>[2] = []) =>
      readView<T>(id, method, args, source);
    try {
      const [sharePrice, totalAssets, totalShares, sleeve, navStale, suspended, allowlistOn] =
        await Promise.all([
          v<bigint>(VAULT_ID, "share_price"),
          v<bigint>(VAULT_ID, "total_assets"),
          v<bigint>(VAULT_ID, "total_shares"),
          v<bigint>(VAULT_ID, "sleeve_balance"),
          v<boolean>(VAULT_ID, "is_nav_stale"),
          v<boolean>(VAULT_ID, "redemptions_suspended"),
          v<boolean>(VAULT_ID, "allowlist_enabled"),
        ]);
      setVault({ sharePrice, totalAssets, totalShares, sleeve, navStale, suspended, allowlistOn });

      if (address) {
        const who = scAddress(address);
        const [baus, allowed, redemption] = await Promise.all([
          v<bigint>(TOKEN_ID, "balance", [who]),
          v<boolean>(VAULT_ID, "is_allowed", [who]),
          v<Redemption>(VAULT_ID, "get_redemption", [who]),
        ]);
        let usdc: bigint | null = null;
        try {
          usdc = await v<bigint>(USDC_ID, "balance", [who]);
        } catch {
          usdc = null; // most likely: no trustline yet
        }
        setWallet({ usdc, baus, allowed, redemption });
      }
    } catch (e) {
      say("err", friendlyError(e));
    }
  }, [address]);

  useEffect(() => {
    refresh();
    const t = setInterval(refresh, 15_000);
    return () => clearInterval(t);
  }, [refresh]);

  const run = async (label: string, fn: () => Promise<unknown>, done: string) => {
    if (!address) return;
    setNote(null);
    setBusy(label);
    try {
      await fn();
      say("ok", done);
      await refresh();
    } catch (e) {
      say("err", friendlyError(e));
    } finally {
      setBusy(null);
    }
  };

  const deposit = () => {
    const amt = parseAmount(depositIn);
    if (amt === null || amt <= 0n) return say("err", "Enter a positive USDC amount, up to 7 decimals.");
    run(
      "deposit",
      () => invoke(VAULT_ID, "subscribe", [scAddress(address!), scI128(amt)], address!, setBusy),
      `Deposited ${fmt(amt)} USDC — baUSD minted at the current share price.`
    ).then(() => setDepositIn(""));
  };

  const requestRedeem = () => {
    const shares = parseAmount(redeemIn);
    if (shares === null || shares <= 0n) return say("err", "Enter a positive baUSD amount, up to 7 decimals.");
    run(
      "redeem",
      () =>
        invoke(VAULT_ID, "request_redemption", [scAddress(address!), scI128(shares)], address!, setBusy),
      `Redemption requested for ${fmt(shares)} baUSD — the shares are escrowed until you claim.`
    ).then(() => setRedeemIn(""));
  };

  const claim = () =>
    run(
      "claim",
      () => invoke(VAULT_ID, "claim_redemption", [scAddress(address!)], address!, setBusy),
      "Claimed. USDC paid at claim-time NAV; anything the sleeve couldn't cover stays queued."
    );

  const cancel = () =>
    run(
      "cancel",
      () => invoke(VAULT_ID, "cancel_redemption", [scAddress(address!)], address!, setBusy),
      "Redemption cancelled — your baUSD is back in your wallet."
    );

  const trustline = () =>
    run(
      "trustline",
      () => addUsdcTrustline(address!, USDC_CODE, USDC_ISSUER, setBusy),
      "USDC trustline added. You can now hold and deposit USDC."
    );

  const positionValue = useMemo(() => {
    if (!vault || !wallet || vault.totalShares === 0n) return 0n;
    return (wallet.baus * (vault.totalAssets + 1n)) / (vault.totalShares + 1n);
  }, [vault, wallet]);

  const claimableAt = wallet?.redemption
    ? new Date(Number(wallet.redemption.claimable_at) * 1000)
    : null;
  const matured = claimableAt ? claimableAt.getTime() <= Date.now() : false;

  return (
    <div className="shell">
      <header>
        <div>
          <span className="eyebrow">Ballast Re · Stellar testnet</span>
          <h1>baUSD</h1>
          <p className="tag">Reinsurance-backed yield. Deposit USDC, hold appreciating shares, redeem at attested NAV.</p>
        </div>
        {address ? (
          <span className="addr mono" title={address}>{short(address)}</span>
        ) : (
          <button className="primary" onClick={connect}>Connect Freighter</button>
        )}
      </header>

      {note && <div className={`note ${note.kind}`} role="status">{note.text}</div>}
      {busy && <div className="note run" role="status">{busy}</div>}

      <section className="grid">
        <div className="card">
          <h2>Vault</h2>
          <dl>
            <div><dt>Share price</dt><dd className="mono big">{vault ? fmt(vault.sharePrice) : "—"}</dd></div>
            <div><dt>Attested NAV</dt><dd className="mono">{vault ? fmt(vault.totalAssets) : "—"} USDC</dd></div>
            <div><dt>Shares outstanding</dt><dd className="mono">{vault ? fmt(vault.totalShares) : "—"}</dd></div>
            <div><dt>Liquidity sleeve</dt><dd className="mono">{vault ? fmt(vault.sleeve) : "—"} USDC</dd></div>
          </dl>
          <div className="flags">
            {vault?.navStale && <span className="flag warn">NAV stale — deposits paused</span>}
            {vault?.suspended && <span className="flag warn">Redemptions suspended</span>}
            {vault?.allowlistOn && !wallet?.allowed && address && (
              <span className="flag warn">Wallet not allowlisted</span>
            )}
          </div>
        </div>

        <div className="card">
          <h2>Your position</h2>
          {address ? (
            <>
              <dl>
                <div><dt>baUSD</dt><dd className="mono big">{wallet ? fmt(wallet.baus) : "—"}</dd></div>
                <div><dt>Value at NAV</dt><dd className="mono">{wallet ? fmt(positionValue) : "—"} USDC</dd></div>
                <div>
                  <dt>USDC</dt>
                  <dd className="mono">
                    {wallet ? (wallet.usdc === null ? "no trustline" : fmt(wallet.usdc)) : "—"}
                  </dd>
                </div>
              </dl>
              {wallet?.usdc === null && (
                <button onClick={trustline} disabled={!!busy}>Add USDC trustline</button>
              )}
            </>
          ) : (
            <p className="empty">Connect Freighter to see balances and transact.</p>
          )}
        </div>
      </section>

      {address && !wrongNetwork && (
        <section className="grid">
          <div className="card">
            <h2>Deposit</h2>
            <p className="hint">USDC in, baUSD out at the current share price, rounded in the vault's favour.</p>
            <div className="row">
              <input
                inputMode="decimal"
                placeholder="0.0000000"
                value={depositIn}
                onChange={(e) => setDepositIn(e.target.value)}
                aria-label="USDC amount to deposit"
              />
              <button className="primary" onClick={deposit} disabled={!!busy}>Deposit USDC</button>
            </div>
          </div>

          <div className="card">
            <h2>Redeem</h2>
            {wallet?.redemption ? (
              <>
                <p className="hint">
                  <b className="mono">{fmt(wallet.redemption.shares)}</b> baUSD escrowed ·{" "}
                  {matured ? "matured — claim any time" : `claimable ${claimableAt!.toLocaleString()}`}
                </p>
                <div className="row">
                  <button className="primary" onClick={claim} disabled={!!busy || !matured}>
                    Claim USDC
                  </button>
                  <button onClick={cancel} disabled={!!busy}>Cancel &amp; return shares</button>
                </div>
              </>
            ) : (
              <>
                <p className="hint">Request first; claim after the notice period at claim-time NAV. One request at a time.</p>
                <div className="row">
                  <input
                    inputMode="decimal"
                    placeholder="0.0000000"
                    value={redeemIn}
                    onChange={(e) => setRedeemIn(e.target.value)}
                    aria-label="baUSD amount to redeem"
                  />
                  <button className="primary" onClick={requestRedeem} disabled={!!busy}>
                    Request redemption
                  </button>
                </div>
              </>
            )}
          </div>
        </section>
      )}

      <footer>
        <span className="mono">vault {short(VAULT_ID)}</span>
        <span className="mono">baUSD {short(TOKEN_ID)}</span>
        <a href={`https://stellar.expert/explorer/testnet/contract/${VAULT_ID}`} target="_blank" rel="noreferrer">
          view on stellar.expert
        </a>
        <span>Testnet only · unaudited · test USDC has no value</span>
      </footer>
    </div>
  );
}
