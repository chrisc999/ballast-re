// Thin layer over stellar-sdk + Freighter: read-only views via simulation, and a
// single sign-and-submit path for state-changing calls.
import {
  Asset,
  BASE_FEE,
  Contract,
  Operation,
  TransactionBuilder,
  nativeToScVal,
  scValToNative,
  rpc,
  xdr,
} from "@stellar/stellar-sdk";
import { signTransaction } from "@stellar/freighter-api";
import { NETWORK_PASSPHRASE, RPC_URL, VAULT_ERRORS } from "./config";

export const server = new rpc.Server(RPC_URL);

export const scAddress = (a: string) => nativeToScVal(a, { type: "address" });
export const scI128 = (v: bigint) => nativeToScVal(v, { type: "i128" });

/** Read-only contract view: simulate and decode, no signature involved. */
export async function readView<T>(
  contractId: string,
  method: string,
  args: xdr.ScVal[],
  sourceAccount: string
): Promise<T> {
  const account = await server.getAccount(sourceAccount);
  const tx = new TransactionBuilder(account, {
    fee: BASE_FEE,
    networkPassphrase: NETWORK_PASSPHRASE,
  })
    .addOperation(new Contract(contractId).call(method, ...args))
    .setTimeout(60)
    .build();
  const sim = await server.simulateTransaction(tx);
  if (rpc.Api.isSimulationSuccess(sim) && sim.result?.retval) {
    return scValToNative(sim.result.retval) as T;
  }
  throw new Error(friendlyError(sim));
}

/** Sign with Freighter and submit; resolves once the transaction is final. */
export async function invoke(
  contractId: string,
  method: string,
  args: xdr.ScVal[],
  signer: string,
  onPhase?: (phase: string) => void
): Promise<unknown> {
  onPhase?.("Simulating…");
  const account = await server.getAccount(signer);
  const tx = new TransactionBuilder(account, {
    fee: BASE_FEE,
    networkPassphrase: NETWORK_PASSPHRASE,
  })
    .addOperation(new Contract(contractId).call(method, ...args))
    .setTimeout(120)
    .build();

  // prepareTransaction simulates, surfaces contract errors early, and attaches
  // the Soroban footprint + resource fee.
  let prepared;
  try {
    prepared = await server.prepareTransaction(tx);
  } catch (e) {
    throw new Error(friendlyError(e));
  }

  onPhase?.("Waiting for your signature in Freighter…");
  const signed = await signTransaction(prepared.toXDR(), {
    networkPassphrase: NETWORK_PASSPHRASE,
    address: signer,
  });
  if (signed.error) throw new Error(signed.error.message ?? "Signing was declined.");

  onPhase?.("Submitting…");
  const sendRes = await server.sendTransaction(
    TransactionBuilder.fromXDR(signed.signedTxXdr, NETWORK_PASSPHRASE)
  );
  if (sendRes.status === "ERROR") {
    throw new Error(friendlyError(sendRes.errorResult ?? sendRes));
  }

  onPhase?.("Confirming…");
  for (let i = 0; i < 30; i++) {
    await new Promise((r) => setTimeout(r, 2000));
    const res = await server.getTransaction(sendRes.hash);
    if (res.status === "SUCCESS") {
      return res.returnValue ? scValToNative(res.returnValue) : null;
    }
    if (res.status === "FAILED") {
      throw new Error(friendlyError(res));
    }
  }
  throw new Error("Timed out waiting for confirmation. Check the transaction on stellar.expert.");
}

/** Classic change-trust op so the account can hold the USDC asset. */
export async function addUsdcTrustline(
  signer: string,
  code: string,
  issuer: string,
  onPhase?: (phase: string) => void
) {
  const account = await server.getAccount(signer);
  const tx = new TransactionBuilder(account, {
    fee: BASE_FEE,
    networkPassphrase: NETWORK_PASSPHRASE,
  })
    .addOperation(Operation.changeTrust({ asset: new Asset(code, issuer) }))
    .setTimeout(120)
    .build();

  onPhase?.("Waiting for your signature in Freighter…");
  const signed = await signTransaction(tx.toXDR(), {
    networkPassphrase: NETWORK_PASSPHRASE,
    address: signer,
  });
  if (signed.error) throw new Error(signed.error.message ?? "Signing was declined.");

  onPhase?.("Submitting…");
  const sendRes = await server.sendTransaction(
    TransactionBuilder.fromXDR(signed.signedTxXdr, NETWORK_PASSPHRASE)
  );
  if (sendRes.status === "ERROR") throw new Error("The trustline transaction was rejected.");
  onPhase?.("Confirming…");
  for (let i = 0; i < 15; i++) {
    await new Promise((r) => setTimeout(r, 2000));
    const res = await server.getTransaction(sendRes.hash);
    if (res.status === "SUCCESS") return;
    if (res.status === "FAILED") throw new Error("The trustline transaction failed.");
  }
}

/** Map a raw simulation/submission failure onto the vault's typed errors. */
export function friendlyError(raw: unknown): string {
  const text =
    typeof raw === "string" ? raw : (raw as Error)?.message ?? JSON.stringify(raw);
  const m = text.match(/Error\(Contract, #(\d+)\)/);
  if (m) {
    const known = VAULT_ERRORS[Number(m[1])];
    if (known) return known;
    return `The contract refused the call (error #${m[1]}).`;
  }
  if (/trustline/i.test(text) || /op_no_trust/i.test(text)) {
    return "This account has no USDC trustline yet — add one first.";
  }
  if (/underfunded/i.test(text)) return "Not enough USDC for this amount.";
  return text.length > 220 ? text.slice(0, 220) + "…" : text;
}
