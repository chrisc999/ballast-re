// Contract addresses come straight from the deploy artifact so the app can never
// drift from what is actually on testnet. Redeploying and rebuilding is the whole
// update story.
import deployment from "../../deploy/testnet.json";

export const NETWORK_PASSPHRASE = "Test SDF Network ; September 2015";
export const RPC_URL = "https://soroban-testnet.stellar.org";
export const FRIENDBOT_URL = "https://friendbot.stellar.org";

export const VAULT_ID = deployment.vault;
export const TOKEN_ID = deployment.token;
export const USDC_ID = deployment.usdc_sac;
export const STRATEGY_ID = deployment.strategy;
/** The DeFindex vault (created from DeFindex's public factory) that allocates to baUSD. */
export const DEFINDEX_VAULT_ID = deployment.defindex_vault;
/** baUSD's SEP-40 price feed: publishes the vault's attested share price. */
export const PRICE_FEED_ID = deployment.oracle;
export const EXPLORER = "https://stellar.expert/explorer/testnet";
/** Issuer of the mock USDC classic asset; accounts hold it via a trustline,
 *  exactly as they would real USDC. */
export const USDC_ISSUER = deployment.deployer;
export const USDC_CODE = "USDC";

/** baUSD and USDC both use 7 decimals. */
export const DECIMALS = 7;
export const SCALE = 10_000_000n;

/** Vault contract error codes → what the user should actually be told. */
export const VAULT_ERRORS: Record<number, string> = {
  3: "The vault is paused. Try again once the guardian resumes operations.",
  4: "That amount isn't valid. Enter a positive number.",
  5: "The first deposit into the vault must be at least 1 baUSD (1.0000000).",
  6: "This deposit is too small to mint any shares at the current price.",
  9: "You already have a pending redemption. Claim or cancel it first.",
  10: "You don't have a pending redemption to act on.",
  11: "Your redemption hasn't matured yet. Wait out the notice period, then claim.",
  12: "The vault's liquidity sleeve can't cover this right now. Part of your claim may remain queued — try again later.",
  17: "The vault's NAV attestation is stale, so new deposits are on hold. Existing holders can still redeem.",
  25: "Redemptions are temporarily suspended. You can still cancel a pending request.",
  26: "This wallet isn't on the allowlist. Complete onboarding to subscribe or redeem.",
};
