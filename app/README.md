# baUSD web console

Deposit/redeem front end for the baUSD vault on Stellar testnet. React + Vite,
signing via [Freighter](https://freighter.app).

```bash
npm install
npm run dev
```

Contract addresses are imported from `../deploy/testnet.json` at build time, so the
app always matches the live deployment. Freighter must be set to **Testnet**.
