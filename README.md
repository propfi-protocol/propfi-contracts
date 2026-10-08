# PropFi Protocol — Smart Contracts

PropFi is an on-chain real-estate investment protocol built on [Stellar Soroban](https://soroban.stellar.org/). It tokenises real-world properties into tradeable fractional shares, distributes rental yield to holders, enables mortgage lending against property equity, and governs all protocol parameters through on-chain voting.

---

## Table of Contents

- [Architecture Overview](#architecture-overview)
- [Contracts](#contracts)
- [Key Protocol Flows](#key-protocol-flows)
- [Security Model](#security-model)
- [Getting Started](#getting-started)
- [Running Tests](#running-tests)
- [Deployment](#deployment)
- [Indexer](#indexer)
- [Contributing](#contributing)

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                        PropFi Protocol                          │
│                                                                 │
│  ┌──────────────────┐        ┌──────────────────────────────┐  │
│  │  ComplianceReg.  │◄───────│  PropertyRegistry            │  │
│  │  (KYC / AML)     │        │  (property NFTs + valuation) │  │
│  └──────────────────┘        └──────────────┬───────────────┘  │
│           │                                 │                  │
│           │  compliance gate                │ property data    │
│           ▼                                 ▼                  │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │                    FractionVault                         │  │
│  │         (buy / sell / transfer fractional shares)        │  │
│  └───────────────┬──────────────────────────────────────────┘  │
│                  │ balance checkpoints                          │
│                  ▼                                              │
│  ┌───────────────────────┐    ┌──────────────────────────────┐ │
│  │   RentDistributor     │    │       MortgagePool           │ │
│  │  (pro-rata yield)     │    │  (LTV-gated lending / liq.)  │ │
│  └───────────────────────┘    └──────────────────────────────┘ │
│                                         ▲                       │
│  ┌───────────────────────┐              │ price feed            │
│  │    OracleAdapter      │──────────────┘                      │
│  │  (weighted TWAP)      │                                      │
│  └───────────────────────┘                                      │
│                                                                 │
│  ┌───────────────────────┐    ┌──────────────────────────────┐ │
│  │     Governance        │    │      PaymentBridge           │ │
│  │  (proposals + voting) │    │  (cross-border remittance)   │ │
│  └───────────────────────┘    └──────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

All contracts are written in Rust targeting `wasm32-unknown-unknown` and deployed on the Stellar Soroban VM.

---

## Contracts

| Contract | Crate | Description |
|---|---|---|
| **ComplianceRegistry** | `propfi-compliance-registry` | KYC/AML attestation store. Records ZK-proof hashes (never raw PII), manages per-jurisdiction expiry and revocation. All transfers and investments gate against this. |
| **PropertyRegistry** | `propfi-property-registry` | Tokenises real-world properties as on-chain assets. Oracle-validated valuations, compliance-gated ownership transfers, status lifecycle (Active / Inactive / UnderMaintenance). |
| **FractionVault** | `propfi-fraction-vault` | Fractional ownership of tokenised properties. Mint, buy, sell, and transfer fractions. Enforces compliance, supply caps, and property Active status. Checkpoints yield on every balance change. |
| **RentDistributor** | `propfi-rent-distributor` | Pro-rata rent distribution using a per-share accumulator (1e12 scaling). Deposits rent, computes pending yield, and pays out on claim. |
| **MortgagePool** | `propfi-mortgage-pool` | Permissionless lending against property equity. Max 70 % LTV; automatic liquidation at 80 % LTV. LP deposits provide liquidity; interest accrues continuously. |
| **OracleAdapter** | `propfi-oracle-adapter` | Multi-source weighted-average price oracle with TWAP support. Strict staleness enforcement via `get_price_strict()`. |
| **Governance** | `propfi-governance` | On-chain proposal lifecycle. Fraction holders vote proportionally. 48 h voting window + 24 h timelock before execution. Quorum configurable by admin. |
| **PaymentBridge** | `propfi-payment-bridge` | Cross-border payment and remittance layer. Single and batch sends across registered anchor assets with configurable fee. |

### Shared Types (`propfi-types`)

Common structs shared across contracts: `PropertyData`, `PropertyStatus`, `LoanData`, `LoanStatus`, `HealthFactor`, `PriceData`, `JurisdictionRules`, `PathQuote`.

---

## Key Protocol Flows

### 1. Onboarding an investor

```
Admin ──► ComplianceRegistry.attest(user, proof_hash, "US", 365)
```

### 2. Tokenising a property

```
Admin ──► PropertyRegistry.register_property(owner, valuation, doc_hash, "US")
       └─► PropertyRegistry.update_valuation(prop_id, new_val, oracle, asset)

Admin ──► FractionVault.fractionalize(prop_id, total_supply, price, token, ...)
```

### 3. Buying / selling fractions

```
Investor ──► FractionVault.buy_fraction(buyer, prop_id, amount)
             │  checks: property Active, compliance, supply cap
             └─► token.transfer(buyer → vault)

Investor ──► FractionVault.sell_fraction(seller, prop_id, amount, min_price)
             └─► token.transfer(vault → seller)
```

### 4. Rent distribution

```
Landlord ──► RentDistributor.deposit_rent(sender, prop_id, amount, token)
                            (yield-per-share accumulator updated)

Investor ──► RentDistributor.pending_yield(investor, prop_id)  ← view
         └─► RentDistributor.claim(prop_id, investor)
                            (tokens transferred to investor)
```
> Balance checkpoints are triggered automatically by `FractionVault` on every buy/sell/transfer, so yield accrual is always accurate regardless of when the investor claims.

### 5. Mortgage loan

```
LP       ──► MortgagePool.deposit_liquidity(lp, amount)

Borrower ──► MortgagePool.open_loan(borrower, prop_id, amount)
             │  checks: property Active, owner == borrower, LTV ≤ 70 %
             └─► token.transfer(vault → borrower)

Borrower ──► MortgagePool.repay(borrower, loan_id, amount)

Anyone   ──► MortgagePool.liquidate(liquidator, loan_id)
             (only if current LTV > 80 %)
```

### 6. Governance

```
Holder  ──► Governance.propose(proposer, action_type, calldata, description)
            (requires ≥ 1 fraction)

Holders ──► Governance.vote(voter, proposal_id, support)
            (voting power = total fractions held across tracked properties)

Anyone  ──► Governance.execute(proposal_id)
            (after voting_end + 24 h timelock, if for_votes > against_votes and quorum met)
```

---

## Security Model

### Compliance gating
Every fraction purchase and ownership transfer checks `ComplianceRegistry.is_compliant(user, jurisdiction)`. Attestations expire and can be revoked by the admin.

### Oracle staleness
`PropertyRegistry.update_valuation()` calls `OracleAdapter.get_price_strict()`, which returns a hard error if the price data is older than the configured staleness threshold (default: 86 400 s). Stale data never silently updates property valuations.

### Property status
`FractionVault.buy_fraction()` and `MortgagePool.open_loan()` both reject properties that are not `Active`. Setting a property `Inactive` or `UnderMaintenance` immediately blocks new purchases and loans against it.

### Two-step admin transfer
Admin keys for `ComplianceRegistry`, `PropertyRegistry`, and `FractionVault` can be transferred via a two-step `propose_admin()` / `accept_admin()` pattern. A nomination alone does nothing — the new admin must actively confirm, preventing accidental transfers to wrong addresses.

### Liquidation
Loans become liquidatable when current LTV (principal + accrued interest / property valuation) exceeds 80 %. Anyone can call `liquidate()`, making liquidation permissionless and resistant to admin capture.

### Pause mechanism
`MortgagePool` has an admin-controlled pause that blocks `open_loan()` and `deposit_liquidity()` during emergencies. Repayments and liquidations remain open so borrowers are never trapped.

### Storage TTL
All contracts call `env.storage().instance().extend_ttl()` on every public entry point (threshold: ~30 days, extends to: ~60 days). This prevents on-chain state from silently expiring on the Soroban ledger.

---

## Getting Started

### Prerequisites

| Tool | Version |
|---|---|
| Rust | stable (see `rust-toolchain.toml`) |
| `wasm32-unknown-unknown` target | via `rustup target add wasm32-unknown-unknown` |
| Stellar CLI (`stellar`) | latest |
| Node.js | 20+ |
| Docker + Docker Compose | for indexer local dev |

### Initial setup

```bash
# Clone and enter the repo
git clone <repo-url>
cd propfi-contracts

# Install all dependencies and generate a testnet key pair
make setup

# Copy environment config
cp .env.example .env
# Edit .env and fill in ADMIN_PUBLIC_KEY and any deployed contract IDs
```

### Environment variables (`.env`)

| Variable | Description |
|---|---|
| `NETWORK` | `testnet` or `mainnet` |
| `SOROBAN_RPC_URL` | Soroban RPC endpoint |
| `ADMIN_KEY_NAME` | Stellar CLI key alias for the deployer |
| `ADMIN_PUBLIC_KEY` | Deployer's public key |
| `STALENESS_THRESHOLD` | Oracle staleness window in seconds (default: 3600) |
| `POSTGRES_*` | Database connection for the indexer |
| `*_CONTRACT_ID` | Written by `deploy.sh` after each deployment |

---

## Running Tests

```bash
# Unit tests for all contracts
cargo test --workspace

# Integration tests only
cargo test -p propfi-integration-tests

# Or via Makefile
make test-contracts
```

The full test suite covers 113 tests across all contracts including:
- Happy-path flows for each contract
- Error cases for every guard (compliance, status, LTV, staleness, supply cap, …)
- Cross-contract integration scenarios (rent distribution, governance lifecycle, mortgage liquidation, …)

---

## Deployment

```bash
# Build WASM artifacts
make build

# Deploy to testnet (writes contract IDs back to .env)
./scripts/deploy.sh --network testnet

# Deploy to mainnet (requires explicit --confirm flag)
./scripts/deploy.sh --network mainnet --confirm
```

The deploy script:
1. Uploads and instantiates each contract in dependency order
2. Wires contracts together (sets `fraction_vault` on `RentDistributor`, sets `property_registry` and `oracle` on `MortgagePool`, etc.)
3. Writes all deployed contract IDs back to `.env`

### Contract dependency order

```
types → compliance_registry
     → oracle_adapter
     → property_registry (needs oracle_adapter)
     → fraction_vault    (needs property_registry, compliance_registry)
     → rent_distributor  (needs fraction_vault)
     → mortgage_pool     (needs property_registry, oracle_adapter)
     → governance        (needs fraction_vault)
     → payment_bridge
```

---

## Indexer

The off-chain indexer (`indexer/`) subscribes to contract events and writes structured data to a PostgreSQL database via Prisma.

```bash
cd indexer
npm install
npm run build
npm start
```

### Indexed events

| Contract | Events |
|---|---|
| PropertyRegistry | `PropertyRegistered`, `ValuationUpdated`, `OwnershipTransferred` |
| FractionVault | `Fractionalized`, `FractionPurchased`, `FractionSold`, `FractionTransferred` |
| RentDistributor | `RentDeposited`, `YieldDistributed`, `YieldClaimed` |
| MortgagePool | `LoanOpened`, `Repaid`, `Liquidated`, `LiquidityDeposited`, `LiquidityWithdrawn` |
| Governance | `ProposalCreated`, `Voted`, `ProposalExecuted` |
| ComplianceRegistry | `Attested`, `Revoked`, `RulesUpdated` |
| OracleAdapter | `PriceUpdated`, `OracleAdded`, `OracleRemoved`, `StaleAlert` |
| PaymentBridge | `PaymentSent`, `BatchDispatched`, `AnchorRegistered` |

---

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for the full contribution guide. Quick summary:

```bash
# Lint
make lint

# Format
make fmt

# Run full test suite before opening a PR
make test
```

All CI checks (build, test, clippy, fmt) must pass. See [`.github/workflows/ci.yml`](./.github/workflows/ci.yml).

---

## License

[MIT](./LICENSE)
