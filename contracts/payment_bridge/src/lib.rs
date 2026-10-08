#![no_std]
//! Cross-border payment and remittance layer. Supports single and batch sends with anchor registration for fiat on/off ramps.
use propfi_types::PathQuote;
use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env, IntoVal, Symbol, Vec};

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum PaymentBridgeError {
    AlreadyInitialized = 1,
    Unauthorized = 2,
    InvalidAmount = 3,
    AssetNotRegistered = 4,
    InsufficientBalance = 5,
}

const FEE_BPS: i128 = 10;

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    Admin,
    AnchorAsset(Symbol),
    Balance(Address, Symbol),
}

/// Bump instance TTL to ~60 days (in ledgers at 5s/ledger) whenever the current
/// TTL falls below ~30 days. Called on every public entry point so active
/// contracts never silently expire their on-chain state.
const INSTANCE_TTL_THRESHOLD: u32 = 518_400;  // 30 days in ledgers
const INSTANCE_TTL_EXTEND_TO: u32 = 1_036_800; // 60 days in ledgers

#[contract]
pub struct PaymentBridge;

#[contractimpl]
impl PaymentBridge {
    /// Sets the admin address. Called once at deployment.
    pub fn initialize(env: Env, admin: Address) -> Result<(), PaymentBridgeError> {
        env.storage().instance().extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
        let existing: Option<Address> = env.storage().instance().get(&DataKey::Admin);
        if existing.is_some() {
            return Err(PaymentBridgeError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        Ok(())
    }

    /// Deposits tokens into the bridge for a given asset.
    pub fn deposit(
        env: Env,
        user: Address,
        asset: Symbol,
        amount: i128,
    ) -> Result<(), PaymentBridgeError> {
        env.storage().instance().extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
        user.require_auth();
        if amount <= 0 {
            return Err(PaymentBridgeError::InvalidAmount);
        }

        let token: Address = env
            .storage()
            .instance()
            .get(&DataKey::AnchorAsset(asset.clone()))
            .ok_or(PaymentBridgeError::AssetNotRegistered)?;

        let bridge = env.current_contract_address();
        env.invoke_contract::<()>(
            &token,
            &Symbol::new(&env, "transfer"),
            Vec::from_array(
                &env,
                [user.to_val(), bridge.to_val(), amount.into_val(&env)],
            ),
        );

        let key = DataKey::Balance(user.clone(), asset.clone());
        let balance: i128 = env.storage().instance().get(&key).unwrap_or(0);
        env.storage().instance().set(&key, &(balance + amount));

        Ok(())
    }

    /// Withdraws tokens from the bridge for a given asset.
    pub fn withdraw(
        env: Env,
        user: Address,
        asset: Symbol,
        amount: i128,
    ) -> Result<(), PaymentBridgeError> {
        env.storage().instance().extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
        user.require_auth();
        if amount <= 0 {
            return Err(PaymentBridgeError::InvalidAmount);
        }

        let key = DataKey::Balance(user.clone(), asset.clone());
        let balance: i128 = env.storage().instance().get(&key).unwrap_or(0);
        if balance < amount {
            return Err(PaymentBridgeError::InsufficientBalance);
        }

        env.storage().instance().set(&key, &(balance - amount));

        let token: Address = env
            .storage()
            .instance()
            .get(&DataKey::AnchorAsset(asset.clone()))
            .ok_or(PaymentBridgeError::AssetNotRegistered)?;

        let bridge = env.current_contract_address();
        env.invoke_contract::<()>(
            &token,
            &Symbol::new(&env, "transfer"),
            Vec::from_array(
                &env,
                [bridge.to_val(), user.to_val(), amount.into_val(&env)],
            ),
        );

        Ok(())
    }

    /// Sends `amount` from one asset to another via path payment.
    pub fn send(
        env: Env,
        from: Address,
        to: Address,
        amount: i128,
        src: Symbol,
        dst: Symbol,
    ) -> Result<(), PaymentBridgeError> {
        env.storage().instance().extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
        from.require_auth();
        if amount <= 0 {
            return Err(PaymentBridgeError::InvalidAmount);
        }

        let src_key = DataKey::Balance(from.clone(), src.clone());
        let src_balance: i128 = env.storage().instance().get(&src_key).unwrap_or(0);
        if src_balance < amount {
            return Err(PaymentBridgeError::InsufficientBalance);
        }
        env.storage()
            .instance()
            .set(&src_key, &(src_balance - amount));

        let dest_amount = if src == dst {
            amount
        } else {
            amount - (amount * FEE_BPS / 10000)
        };

        let dst_key = DataKey::Balance(to.clone(), dst.clone());
        let dst_balance: i128 = env.storage().instance().get(&dst_key).unwrap_or(0);
        env.storage()
            .instance()
            .set(&dst_key, &(dst_balance + dest_amount));

        env.events().publish(
            (Symbol::new(&env, "PaymentSent"), from),
            (to, amount, src, dest_amount, dst),
        );

        Ok(())
    }

    /// Sends payments to multiple recipients in batch.
    pub fn batch_send(
        env: Env,
        from: Address,
        recipients: Vec<(Address, i128)>,
        src: Symbol,
        dst: Symbol,
    ) -> Result<(), PaymentBridgeError> {
        env.storage().instance().extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
        from.require_auth();

        let mut total: i128 = 0;
        for i in 0..recipients.len() {
            let (_to, amt) = recipients.get(i).unwrap();
            if amt <= 0 {
                return Err(PaymentBridgeError::InvalidAmount);
            }
            total = total.checked_add(amt).unwrap();
        }

        let src_key = DataKey::Balance(from.clone(), src.clone());
        let src_balance: i128 = env.storage().instance().get(&src_key).unwrap_or(0);
        if src_balance < total {
            return Err(PaymentBridgeError::InsufficientBalance);
        }
        env.storage()
            .instance()
            .set(&src_key, &(src_balance - total));

        for i in 0..recipients.len() {
            let (to, amt) = recipients.get(i).unwrap();

            let dest_amount = if src == dst {
                amt
            } else {
                amt - (amt * FEE_BPS / 10000)
            };

            let dst_key = DataKey::Balance(to.clone(), dst.clone());
            let dst_balance: i128 = env.storage().instance().get(&dst_key).unwrap_or(0);
            env.storage()
                .instance()
                .set(&dst_key, &(dst_balance + dest_amount));
        }

        env.events().publish(
            (Symbol::new(&env, "BatchDispatched"), from),
            (recipients.len(), src, dst),
        );

        Ok(())
    }

    /// Registers an anchor for an asset symbol. Admin-only.
    pub fn register_anchor(
        env: Env,
        asset: Symbol,
        token_address: Address,
    ) -> Result<(), PaymentBridgeError> {
        env.storage().instance().extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(PaymentBridgeError::Unauthorized)?;
        admin.require_auth();

        env.storage()
            .instance()
            .set(&DataKey::AnchorAsset(asset.clone()), &token_address);

        env.events().publish(
            (Symbol::new(&env, "AnchorRegistered"), asset),
            token_address,
        );

        Ok(())
    }

    /// Returns a PathQuote estimating the destination amount, path, and fee for a conversion.
    pub fn estimate_path(
        env: Env,
        src: Symbol,
        dst: Symbol,
        amount: i128,
    ) -> Result<PathQuote, PaymentBridgeError> {
        env.storage().instance().extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
        if amount <= 0 {
            return Err(PaymentBridgeError::InvalidAmount);
        }

        let same = src == dst;

        let dest_amount = if same {
            amount
        } else {
            amount - (amount * FEE_BPS / 10000)
        };

        let mut path: Vec<Address> = Vec::new(&env);
        if let Some(addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::AnchorAsset(src.clone()))
        {
            path.push_back(addr);
        }
        if let Some(addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::AnchorAsset(dst.clone()))
        {
            path.push_back(addr);
        }

        let estimated_fee = if same {
            0i128
        } else {
            amount * FEE_BPS / 10000
        };

        Ok(PathQuote {
            dest_amount,
            path,
            estimated_fee,
        })
    }

    /// Returns the bridge balance of a user for a given asset.
    pub fn get_balance(env: Env, user: Address, asset: Symbol) -> i128 {
        env.storage().instance().extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
        env.storage()
            .instance()
            .get(&DataKey::Balance(user, asset))
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{symbol_short, Env};

    fn setup() -> (
        Env,
        Address,
        Address,
        PaymentBridgeClient<'static>,
        Address,
        Address,
        Symbol,
        Symbol,
    ) {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        let contract_id = env.register_contract(None, PaymentBridge);
        let client = PaymentBridgeClient::new(&env, &contract_id);
        client.initialize(&admin);

        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let usdc = symbol_short!("USDC");
        let xlm = symbol_short!("XLM");

        client.register_anchor(&usdc, &token_a);
        client.register_anchor(&xlm, &token_b);

        (env, admin, user, client, token_a, token_b, usdc, xlm)
    }

    #[test]
    fn test_initialize() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let contract_id = env.register_contract(None, PaymentBridge);
        let client = PaymentBridgeClient::new(&env, &contract_id);
        client.initialize(&admin);
    }

    #[test]
    fn test_double_initialize_returns_error() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let contract_id = env.register_contract(None, PaymentBridge);
        let client = PaymentBridgeClient::new(&env, &contract_id);
        client.initialize(&admin);
        let result = client.try_initialize(&admin);
        assert!(result.is_err());
    }

    #[test]
    fn test_deposit_and_get_balance() {
        let (env, _admin, user, client, token_a, _token_b, usdc, _xlm) = setup();
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token_a);
        sac.mint(&user, &50_000i128);

        client.deposit(&user, &usdc, &10_000i128);

        assert_eq!(client.get_balance(&user, &usdc), 10_000);
    }

    #[test]
    fn test_deposit_unregistered_asset_returns_error() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        let contract_id = env.register_contract(None, PaymentBridge);
        let client = PaymentBridgeClient::new(&env, &contract_id);
        client.initialize(&admin);

        let bad_asset = symbol_short!("BAD");
        let result = client.try_deposit(&user, &bad_asset, &100i128);
        assert!(result.is_err());
    }

    #[test]
    fn test_send_cross_asset() {
        let (env, _admin, user, client, token_a, _token_b, usdc, xlm) = setup();
        let recipient = Address::generate(&env);

        let sac_a = soroban_sdk::token::StellarAssetClient::new(&env, &token_a);
        sac_a.mint(&user, &50_000i128);
        client.deposit(&user, &usdc, &20_000i128);

        client.send(&user, &recipient, &10_000i128, &usdc, &xlm);

        let fee = 10_000 * FEE_BPS / 10000;
        let expected_dest = 10_000 - fee;

        assert_eq!(client.get_balance(&user, &usdc), 10_000);
        assert_eq!(client.get_balance(&recipient, &xlm), expected_dest);
    }

    #[test]
    fn test_send_insufficient_balance_returns_error() {
        let (env, _admin, user, client, _token_a, _token_b, usdc, xlm) = setup();
        let recipient = Address::generate(&env);
        let result = client.try_send(&user, &recipient, &100i128, &usdc, &xlm);
        assert!(result.is_err());
    }

    #[test]
    fn test_send_zero_amount_returns_error() {
        let (env, _admin, user, client, _token_a, _token_b, usdc, xlm) = setup();
        let recipient = Address::generate(&env);
        let result = client.try_send(&user, &recipient, &0i128, &usdc, &xlm);
        assert!(result.is_err());
    }

    #[test]
    fn test_batch_send() {
        let (env, _admin, user, client, token_a, _token_b, usdc, xlm) = setup();
        let recipient1 = Address::generate(&env);
        let recipient2 = Address::generate(&env);

        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token_a);
        sac.mint(&user, &100_000i128);
        client.deposit(&user, &usdc, &50_000i128);

        let recipients = Vec::from_array(
            &env,
            [
                (recipient1.clone(), 10_000i128),
                (recipient2.clone(), 5_000i128),
            ],
        );

        client.batch_send(&user, &recipients, &usdc, &xlm);

        let fee1 = 10_000 * FEE_BPS / 10000;
        let fee2 = 5_000 * FEE_BPS / 10000;

        assert_eq!(client.get_balance(&user, &usdc), 35_000);
        assert_eq!(client.get_balance(&recipient1, &xlm), 10_000 - fee1);
        assert_eq!(client.get_balance(&recipient2, &xlm), 5_000 - fee2);
    }

    #[test]
    fn test_estimate_path_same_asset() {
        let (_env, _admin, _user, client, _token_a, _token_b, usdc, _xlm) = setup();

        let quote = client.estimate_path(&usdc, &usdc, &10_000i128);

        assert_eq!(quote.dest_amount, 10_000);
        assert_eq!(quote.estimated_fee, 0);
    }

    #[test]
    fn test_estimate_path_cross_asset() {
        let (_env, _admin, _user, client, _token_a, _token_b, usdc, xlm) = setup();

        let quote = client.estimate_path(&usdc, &xlm, &10_000i128);

        let expected_dest = 10_000 - (10_000 * FEE_BPS / 10000);
        assert_eq!(quote.dest_amount, expected_dest);
        assert_eq!(quote.estimated_fee, 10_000 * FEE_BPS / 10000);
    }
}
