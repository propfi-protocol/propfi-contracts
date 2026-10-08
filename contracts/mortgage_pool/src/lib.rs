#![no_std]
//! Permissionless on-chain lending against tokenized property equity. LTV-gated with automated liquidation at 80% threshold.
use propfi_types::{HealthFactor, LoanData, LoanStatus, PropertyData, PropertyStatus};
use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env, IntoVal, Symbol, Vec};

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum MortgagePoolError {
    AlreadyInitialized = 1,
    Unauthorized = 2,
    LoanNotFound = 3,
    LoanNotActive = 4,
    OnlyPropertyOwner = 5,
    LoanExceedsMaxLtv = 6,
    InsufficientPoolLiquidity = 7,
    InsufficientLpBalance = 8,
    LoanIsHealthy = 9,
    ContractPaused = 10,
    PropertyNotActive = 11,
}

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    Admin,
    Loan(u64), // loan_id -> LoanData
    LoanCounter,
    Liquidity(Address), // LP address -> balance
    TotalLiquidity,
    LiquidityToken,
    PropertyRegistry,
    OracleAdapter,
    Paused,
}

const MAX_LTV_BPS: u32 = 7000; // 70%
const LIQUIDATION_THRESHOLD_BPS: u32 = 8000; // 80%
const INTEREST_RATE_BPS: u32 = 500; // 5% annual
const SECONDS_PER_YEAR: u64 = 31_536_000;

#[contract]
pub struct MortgagePool;

#[contractimpl]
impl MortgagePool {
    /// Sets admin, token, property registry, and oracle. Called once at deployment.
    pub fn initialize(
        env: Env,
        admin: Address,
        token: Address,
        property_reg: Address,
        oracle: Address,
    ) -> Result<(), MortgagePoolError> {
        let existing: Option<Address> = env.storage().instance().get(&DataKey::Admin);
        if existing.is_some() {
            return Err(MortgagePoolError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::LiquidityToken, &token);
        env.storage()
            .instance()
            .set(&DataKey::PropertyRegistry, &property_reg);
        env.storage()
            .instance()
            .set(&DataKey::OracleAdapter, &oracle);
        env.storage().instance().set(&DataKey::LoanCounter, &0u64);
        env.storage()
            .instance()
            .set(&DataKey::TotalLiquidity, &0i128);
        env.storage().instance().set(&DataKey::Paused, &false);
        Ok(())
    }

    /// Pauses the contract. Admin-only. Blocks open_loan and deposit_liquidity.
    pub fn pause(env: Env) -> Result<(), MortgagePoolError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(MortgagePoolError::Unauthorized)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        env.events()
            .publish((Symbol::new(&env, "Paused"),), env.ledger().timestamp());
        Ok(())
    }

    /// Unpauses the contract. Admin-only.
    pub fn unpause(env: Env) -> Result<(), MortgagePoolError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(MortgagePoolError::Unauthorized)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &false);
        env.events()
            .publish((Symbol::new(&env, "Unpaused"),), env.ledger().timestamp());
        Ok(())
    }

    /// Returns whether the contract is paused.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// Opens a new loan against a property. Borrower must authorize. Enforces max 70% LTV.
    pub fn open_loan(
        env: Env,
        borrower: Address,
        prop_id: u64,
        amount: i128,
    ) -> Result<u64, MortgagePoolError> {
        if Self::is_paused(env.clone()) {
            return Err(MortgagePoolError::ContractPaused);
        }
        borrower.require_auth();

        let property_reg: Address = env
            .storage()
            .instance()
            .get(&DataKey::PropertyRegistry)
            .ok_or(MortgagePoolError::Unauthorized)?;
        let property: PropertyData = env.invoke_contract(
            &property_reg,
            &Symbol::new(&env, "get_property"),
            Vec::from_array(&env, [prop_id.into_val(&env)]),
        );

        if property.owner != borrower {
            return Err(MortgagePoolError::OnlyPropertyOwner);
        }

        // Only allow loans against actively-listed properties
        if property.status != PropertyStatus::Active {
            return Err(MortgagePoolError::PropertyNotActive);
        }

        let valuation = property.valuation;
        let max_loan = valuation * (MAX_LTV_BPS as i128) / 10000;
        if amount > max_loan {
            return Err(MortgagePoolError::LoanExceedsMaxLtv);
        }

        let total_liq: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalLiquidity)
            .unwrap_or(0);
        if amount > total_liq {
            return Err(MortgagePoolError::InsufficientPoolLiquidity);
        }

        let mut counter: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LoanCounter)
            .ok_or(MortgagePoolError::Unauthorized)?;
        counter += 1;
        env.storage()
            .instance()
            .set(&DataKey::LoanCounter, &counter);

        let now = env.ledger().timestamp();
        let loan = LoanData {
            prop_id,
            borrower: borrower.clone(),
            amount,
            collateral_valuation: valuation,
            ltv_bps: (amount * 10000 / valuation) as u32,
            interest_rate_bps: INTEREST_RATE_BPS,
            created_at: now,
            last_repayment_at: now,
            status: LoanStatus::Active,
        };

        env.storage().instance().set(&DataKey::Loan(counter), &loan);
        env.storage()
            .instance()
            .set(&DataKey::TotalLiquidity, &(total_liq - amount));

        let token: Address = env
            .storage()
            .instance()
            .get(&DataKey::LiquidityToken)
            .ok_or(MortgagePoolError::Unauthorized)?;
        let vault = env.current_contract_address();
        env.invoke_contract::<()>(
            &token,
            &Symbol::new(&env, "transfer"),
            Vec::from_array(
                &env,
                [vault.to_val(), borrower.to_val(), amount.into_val(&env)],
            ),
        );

        env.events().publish(
            (Symbol::new(&env, "LoanOpened"), counter),
            (borrower, prop_id, amount),
        );

        Ok(counter)
    }

    /// Repays `amount` of a loan. Only callable by the borrower.
    pub fn repay(
        env: Env,
        borrower: Address,
        loan_id: u64,
        amount: i128,
    ) -> Result<(), MortgagePoolError> {
        borrower.require_auth();
        let mut loan: LoanData = env
            .storage()
            .instance()
            .get(&DataKey::Loan(loan_id))
            .ok_or(MortgagePoolError::LoanNotFound)?;
        if loan.status != LoanStatus::Active {
            return Err(MortgagePoolError::LoanNotActive);
        }

        let interest = MortgagePool::calculate_interest_internal(env.clone(), &loan);
        let total_due = loan.amount + interest;

        let repayment = if amount > total_due { total_due } else { amount };

        let token: Address = env
            .storage()
            .instance()
            .get(&DataKey::LiquidityToken)
            .ok_or(MortgagePoolError::Unauthorized)?;
        let vault = env.current_contract_address();
        env.invoke_contract::<()>(
            &token,
            &Symbol::new(&env, "transfer"),
            Vec::from_array(
                &env,
                [borrower.to_val(), vault.to_val(), repayment.into_val(&env)],
            ),
        );

        if repayment >= total_due {
            loan.amount = 0;
            loan.status = LoanStatus::Repaid;
        } else {
            // Reduce principal by the amount above interest paid
            if repayment > interest {
                loan.amount -= repayment - interest;
            }
            // Always advance the interest checkpoint so the repaid interest
            // period is not double-charged on the next repayment call.
            loan.last_repayment_at = env.ledger().timestamp();
        }

        env.storage().instance().set(&DataKey::Loan(loan_id), &loan);

        let total_liq: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalLiquidity)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalLiquidity, &(total_liq + repayment));

        env.events().publish(
            (Symbol::new(&env, "Repaid"), loan_id),
            (borrower, repayment),
        );
        Ok(())
    }

    /// Liquidates an underwater loan (LTV > 80%). Callable by anyone.
    pub fn liquidate(
        env: Env,
        liquidator: Address,
        loan_id: u64,
    ) -> Result<(), MortgagePoolError> {
        liquidator.require_auth();
        let mut loan: LoanData = env
            .storage()
            .instance()
            .get(&DataKey::Loan(loan_id))
            .ok_or(MortgagePoolError::LoanNotFound)?;
        if loan.status != LoanStatus::Active {
            return Err(MortgagePoolError::LoanNotActive);
        }

        let health = MortgagePool::loan_health(env.clone(), loan_id)?;
        if health.is_healthy {
            return Err(MortgagePoolError::LoanIsHealthy);
        }

        loan.status = LoanStatus::Liquidated;
        env.storage().instance().set(&DataKey::Loan(loan_id), &loan);

        env.events()
            .publish((Symbol::new(&env, "Liquidated"), loan_id), liquidator);
        Ok(())
    }

    /// Deposits tokens to the liquidity pool. Callable by any LP.
    pub fn deposit_liquidity(env: Env, lp: Address, amount: i128) -> Result<(), MortgagePoolError> {
        if Self::is_paused(env.clone()) {
            return Err(MortgagePoolError::ContractPaused);
        }
        lp.require_auth();

        let token: Address = env
            .storage()
            .instance()
            .get(&DataKey::LiquidityToken)
            .ok_or(MortgagePoolError::Unauthorized)?;
        let vault = env.current_contract_address();
        env.invoke_contract::<()>(
            &token,
            &Symbol::new(&env, "transfer"),
            Vec::from_array(&env, [lp.to_val(), vault.to_val(), amount.into_val(&env)]),
        );

        let key = DataKey::Liquidity(lp.clone());
        let balance: i128 = env.storage().instance().get(&key).unwrap_or(0);
        env.storage().instance().set(&key, &(balance + amount));

        let total_liq: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalLiquidity)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalLiquidity, &(total_liq + amount));

        env.events()
            .publish((Symbol::new(&env, "LiquidityDeposited"),), (lp, amount));
        Ok(())
    }

    /// Withdraws tokens from the liquidity pool. Only allowed when total available liquidity
    /// (not lent out) covers the withdrawal, protecting active borrowers.
    pub fn withdraw_liquidity(
        env: Env,
        lp: Address,
        amount: i128,
    ) -> Result<(), MortgagePoolError> {
        lp.require_auth();

        let key = DataKey::Liquidity(lp.clone());
        let balance: i128 = env.storage().instance().get(&key).unwrap_or(0);
        if balance < amount {
            return Err(MortgagePoolError::InsufficientLpBalance);
        }

        let total_liq: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalLiquidity)
            .unwrap_or(0);
        if total_liq < amount {
            return Err(MortgagePoolError::InsufficientPoolLiquidity);
        }

        env.storage().instance().set(&key, &(balance - amount));
        env.storage()
            .instance()
            .set(&DataKey::TotalLiquidity, &(total_liq - amount));

        let token: Address = env
            .storage()
            .instance()
            .get(&DataKey::LiquidityToken)
            .ok_or(MortgagePoolError::Unauthorized)?;
        let vault = env.current_contract_address();
        env.invoke_contract::<()>(
            &token,
            &Symbol::new(&env, "transfer"),
            Vec::from_array(&env, [vault.to_val(), lp.to_val(), amount.into_val(&env)]),
        );

        env.events()
            .publish((Symbol::new(&env, "LiquidityWithdrawn"),), (lp, amount));
        Ok(())
    }

    /// Returns the HealthFactor for a loan, indicating whether it's at risk of liquidation.
    pub fn loan_health(env: Env, loan_id: u64) -> Result<HealthFactor, MortgagePoolError> {
        let loan: LoanData = env
            .storage()
            .instance()
            .get(&DataKey::Loan(loan_id))
            .ok_or(MortgagePoolError::LoanNotFound)?;

        let property_reg: Address = env
            .storage()
            .instance()
            .get(&DataKey::PropertyRegistry)
            .ok_or(MortgagePoolError::Unauthorized)?;
        let property: PropertyData = env.invoke_contract(
            &property_reg,
            &Symbol::new(&env, "get_property"),
            Vec::from_array(&env, [loan.prop_id.into_val(&env)]),
        );

        let interest = MortgagePool::calculate_interest_internal(env.clone(), &loan);
        let current_debt = loan.amount + interest;
        let current_ltv = (current_debt * 10000 / property.valuation) as u32;

        Ok(HealthFactor {
            ratio: current_ltv,
            is_healthy: current_ltv < LIQUIDATION_THRESHOLD_BPS,
        })
    }

    /// Returns the full LoanData for a given loan ID.
    pub fn get_loan(env: Env, loan_id: u64) -> Result<LoanData, MortgagePoolError> {
        env.storage()
            .instance()
            .get(&DataKey::Loan(loan_id))
            .ok_or(MortgagePoolError::LoanNotFound)
    }

    /// Returns the LP deposit balance for a given address.
    pub fn lp_balance(env: Env, lp: Address) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::Liquidity(lp))
            .unwrap_or(0)
    }

    /// Returns total available liquidity in the pool.
    pub fn total_liquidity(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalLiquidity)
            .unwrap_or(0)
    }

    fn calculate_interest_internal(env: Env, loan: &LoanData) -> i128 {
        let elapsed = env.ledger().timestamp() - loan.last_repayment_at;
        if elapsed == 0 {
            return 0;
        }

        (loan.amount * (loan.interest_rate_bps as i128) * (elapsed as i128))
            / (10000 * (SECONDS_PER_YEAR as i128))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use propfi_oracle_adapter::{OracleAdapter, OracleAdapterClient};
    use propfi_property_registry::{PropertyRegistry, PropertyRegistryClient};
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::{symbol_short, BytesN, Env};

    fn setup() -> (
        Env,
        Address,
        Address,
        MortgagePoolClient<'static>,
        Address,
        Address,
        Address,
    ) {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let property_owner = Address::generate(&env);

        let token = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let prop_reg_id = env.register_contract(None, PropertyRegistry);
        let prop_reg_client = PropertyRegistryClient::new(&env, &prop_reg_id);
        prop_reg_client.initialize(&admin);

        let doc_hash = BytesN::from_array(&env, &[0u8; 32]);
        let _prop_id = prop_reg_client.register_property(
            &property_owner,
            &100_000i128,
            &doc_hash,
            &symbol_short!("US"),
        );

        let oracle_id = env.register_contract(None, OracleAdapter);
        let oracle_client = OracleAdapterClient::new(&env, &oracle_id);
        oracle_client.initialize(&admin, &86400u64);
        oracle_client.add_oracle(&admin, &100u32);
        oracle_client.submit_price(&admin, &Symbol::new(&env, "PROP_USD"), &100_000i128);

        let pool_id = env.register_contract(None, MortgagePool);
        let pool_client = MortgagePoolClient::new(&env, &pool_id);
        pool_client.initialize(&admin, &token, &prop_reg_id, &oracle_id);

        (
            env,
            admin,
            property_owner,
            pool_client,
            token,
            prop_reg_id,
            oracle_id,
        )
    }

    #[test]
    fn test_deposit_and_open_loan() {
        let (env, admin, owner, pool, token, _, _) = setup();
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token);

        sac.mint(&admin, &100_000i128);
        pool.deposit_liquidity(&admin, &50_000i128);

        let loan_id = pool.open_loan(&owner, &1u64, &30_000i128);
        assert_eq!(loan_id, 1);
        let token_client = soroban_sdk::token::TokenClient::new(&env, &token);
        assert_eq!(token_client.balance(&owner), 30_000);
    }

    #[test]
    fn test_ltv_enforcement_returns_error() {
        let (env, admin, owner, pool, token, _, _) = setup();
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        sac.mint(&admin, &100_000i128);
        pool.deposit_liquidity(&admin, &100_000i128);

        let result = pool.try_open_loan(&owner, &1u64, &80_000i128);
        assert!(result.is_err());
    }

    #[test]
    fn test_repay_loan() {
        let (env, admin, owner, pool, token, _, _) = setup();
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        sac.mint(&admin, &100_000i128);
        pool.deposit_liquidity(&admin, &50_000i128);

        let loan_id = pool.open_loan(&owner, &1u64, &20_000i128);

        env.ledger()
            .set_timestamp(env.ledger().timestamp() + SECONDS_PER_YEAR);

        sac.mint(&owner, &1_000i128);
        pool.repay(&owner, &loan_id, &21_000i128);

        let token_client = soroban_sdk::token::TokenClient::new(&env, &token);
        assert_eq!(token_client.balance(&owner), 0);
    }

    #[test]
    fn test_liquidation_health() {
        let (env, admin, owner, pool, token, prop_reg_id, oracle_id) = setup();
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        sac.mint(&admin, &100_000i128);
        pool.deposit_liquidity(&admin, &100_000i128);

        let loan_id = pool.open_loan(&owner, &1u64, &60_000i128);

        let oracle_client = OracleAdapterClient::new(&env, &oracle_id);
        oracle_client.submit_price(&admin, &Symbol::new(&env, "PROP_USD"), &70_000i128);

        let prop_reg_client = PropertyRegistryClient::new(&env, &prop_reg_id);
        prop_reg_client.update_valuation(
            &1u64,
            &70_000i128,
            &oracle_id,
            &Symbol::new(&env, "PROP_USD"),
        );

        let health = pool.loan_health(&loan_id);
        assert!(!health.is_healthy);
        assert!(health.ratio > 8000);

        pool.liquidate(&admin, &loan_id);
    }

    #[test]
    fn test_get_loan() {
        let (env, admin, owner, pool, token, _, _) = setup();
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        sac.mint(&admin, &100_000i128);
        pool.deposit_liquidity(&admin, &50_000i128);

        let loan_id = pool.open_loan(&owner, &1u64, &30_000i128);
        let loan = pool.get_loan(&loan_id);
        assert_eq!(loan.borrower, owner);
        assert_eq!(loan.amount, 30_000);
    }

    #[test]
    fn test_lp_balance() {
        let (env, admin, _owner, pool, token, _, _) = setup();
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        sac.mint(&admin, &100_000i128);
        pool.deposit_liquidity(&admin, &50_000i128);
        assert_eq!(pool.lp_balance(&admin), 50_000);
        assert_eq!(pool.total_liquidity(), 50_000);
    }

    #[test]
    fn test_pause_blocks_open_loan() {
        let (env, admin, owner, pool, token, _, _) = setup();
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        sac.mint(&admin, &100_000i128);
        pool.deposit_liquidity(&admin, &50_000i128);

        pool.pause();
        assert!(pool.is_paused());

        let result = pool.try_open_loan(&owner, &1u64, &30_000i128);
        assert!(result.is_err());

        pool.unpause();
        assert!(!pool.is_paused());
        let loan_id = pool.open_loan(&owner, &1u64, &30_000i128);
        assert_eq!(loan_id, 1);
    }

    #[test]
    fn test_withdraw_liquidity_guard() {
        let (env, admin, owner, pool, token, _, _) = setup();
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        sac.mint(&admin, &100_000i128);
        pool.deposit_liquidity(&admin, &50_000i128);

        // Open a loan consuming 30_000 of the 50_000 pool
        pool.open_loan(&owner, &1u64, &30_000i128);
        // Pool now only has 20_000 free
        assert_eq!(pool.total_liquidity(), 20_000);

        // LP cannot withdraw more than remaining free liquidity
        let result = pool.try_withdraw_liquidity(&admin, &40_000i128);
        assert!(result.is_err());

        // Can withdraw within available liquidity
        pool.withdraw_liquidity(&admin, &10_000i128);
        assert_eq!(pool.lp_balance(&admin), 40_000);
    }

    /// Regression test for the interest double-charge bug.
    ///
    /// Before the fix, a partial repayment that only covered interest (i.e.
    /// `repayment <= interest`) left `last_repayment_at` unchanged.  The next
    /// repayment call would then re-compute interest from the same old baseline,
    /// effectively charging the same interest period twice.
    #[test]
    fn test_partial_interest_repayment_advances_checkpoint() {
        let (env, admin, owner, pool, token, _, _) = setup();
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        sac.mint(&admin, &100_000i128);
        pool.deposit_liquidity(&admin, &50_000i128);

        let loan_id = pool.open_loan(&owner, &1u64, &20_000i128);

        // Advance one year so that interest accrues
        env.ledger()
            .set_timestamp(env.ledger().timestamp() + SECONDS_PER_YEAR);

        // Interest for one year at 5% on 20_000 = 1_000
        // Pay only the interest (1_000) — principal stays at 20_000
        sac.mint(&owner, &1_000i128);
        pool.repay(&owner, &loan_id, &1_000i128);

        let loan_after_interest_pay = pool.get_loan(&loan_id);
        // Principal unchanged
        assert_eq!(loan_after_interest_pay.amount, 20_000);

        // Advance another year; interest should accrue from the NEW checkpoint,
        // NOT from the original loan creation time.
        env.ledger()
            .set_timestamp(env.ledger().timestamp() + SECONDS_PER_YEAR);

        // Now pay the remaining principal + one more year of interest
        sac.mint(&owner, &21_000i128);
        pool.repay(&owner, &loan_id, &21_000i128);

        let loan_final = pool.get_loan(&loan_id);
        assert_eq!(loan_final.status, LoanStatus::Repaid);
        assert_eq!(loan_final.amount, 0);
    }

    #[test]
    fn test_open_loan_inactive_property_returns_error() {
        let (env, admin, owner, pool, token, prop_reg_id, _oracle_id) = setup();
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        sac.mint(&admin, &100_000i128);
        pool.deposit_liquidity(&admin, &100_000i128);

        // Mark the property as Inactive
        let prop_reg_client = PropertyRegistryClient::new(&env, &prop_reg_id);
        prop_reg_client.set_status(
            &1u64,
            &propfi_types::PropertyStatus::Inactive,
        );

        let result = pool.try_open_loan(&owner, &1u64, &30_000i128);
        assert!(result.is_err());
    }
}
