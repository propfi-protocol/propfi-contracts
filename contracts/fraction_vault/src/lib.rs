#![no_std]
//! Manages fractional ownership of tokenized properties. Supports minting fractions, buying/selling on secondary market, and holder tracking.
use propfi_types::{PropertyData, PropertyStatus};
use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env, IntoVal, Symbol, Vec};

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum FractionVaultError {
    AlreadyInitialized = 1,
    Unauthorized = 2,
    PropertyNotFractionalized = 3,
    AlreadyFractionalized = 4,
    InvalidTotalSupply = 5,
    InvalidPrice = 6,
    InvalidAmount = 7,
    ComplianceCheckFailed = 8,
    InsufficientBalance = 9,
    PriceTooLow = 10,
    SupplyExceeded = 11,
    PropertyNotActive = 12,
    NoPendingAdminTransfer = 13,
}

#[derive(Clone, Debug, PartialEq)]
#[contracttype]
pub struct FractionInfo {
    pub total_supply: u128,
    pub price: i128,
    pub payment_token: Address,
    pub property_registry: Address,
    pub compliance_registry: Address,
}

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    Admin,
    FractionInfo(u64),
    Balance(Address, u64),
    HolderCount(u64),
    IsHolder(Address, u64),
    MintedSupply(u64),
    /// Pending admin awaiting acceptance (two-step transfer).
    PendingAdmin,
}

#[contract]
pub struct FractionVault;

#[contractimpl]
impl FractionVault {
    /// Sets the admin address. Called once at deployment.
    pub fn initialize(env: Env, admin: Address) -> Result<(), FractionVaultError> {
        let existing: Option<Address> = env.storage().instance().get(&DataKey::Admin);
        if existing.is_some() {
            return Err(FractionVaultError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        Ok(())
    }

    /// Mints `total_supply` fractions for a property at the given price. Admin-only. Cross-calls PropertyRegistry to validate the property.
    pub fn fractionalize(
        env: Env,
        prop_id: u64,
        total_supply: u128,
        price: i128,
        payment_token: Address,
        property_registry: Address,
        compliance_registry: Address,
    ) -> Result<(), FractionVaultError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(FractionVaultError::Unauthorized)?;
        admin.require_auth();

        if total_supply == 0 {
            return Err(FractionVaultError::InvalidTotalSupply);
        }
        if price <= 0 {
            return Err(FractionVaultError::InvalidPrice);
        }

        if env
            .storage()
            .instance()
            .has(&DataKey::FractionInfo(prop_id))
        {
            return Err(FractionVaultError::AlreadyFractionalized);
        }

        let _property: PropertyData = env.invoke_contract(
            &property_registry,
            &Symbol::new(&env, "get_property"),
            Vec::from_array(&env, [prop_id.into_val(&env)]),
        );

        let info = FractionInfo {
            total_supply,
            price,
            payment_token,
            property_registry,
            compliance_registry,
        };

        env.storage()
            .instance()
            .set(&DataKey::FractionInfo(prop_id), &info);

        env.storage()
            .instance()
            .set(&DataKey::HolderCount(prop_id), &0u32);

        env.storage()
            .instance()
            .set(&DataKey::MintedSupply(prop_id), &0u128);

        env.events().publish(
            (Symbol::new(&env, "Fractionalized"), prop_id),
            (total_supply, price),
        );

        Ok(())
    }

    /// Returns the fraction balance of an investor for a given property.
    pub fn get_balance(env: Env, investor: Address, prop_id: u64) -> u128 {
        env.storage()
            .instance()
            .get(&DataKey::Balance(investor, prop_id))
            .unwrap_or(0)
    }

    /// Returns the total number of unique holders for a property.
    pub fn total_holders(env: Env, prop_id: u64) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::HolderCount(prop_id))
            .unwrap_or(0)
    }

    /// Returns the currently minted supply for a property.
    pub fn minted_supply(env: Env, prop_id: u64) -> u128 {
        env.storage()
            .instance()
            .get(&DataKey::MintedSupply(prop_id))
            .unwrap_or(0)
    }

    /// Returns the FractionInfo struct for a property.
    pub fn get_fraction_info(env: Env, prop_id: u64) -> Result<(u128, i128, Address, Address, Address), FractionVaultError> {
        let info: FractionInfo = env
            .storage()
            .instance()
            .get(&DataKey::FractionInfo(prop_id))
            .ok_or(FractionVaultError::PropertyNotFractionalized)?;
        Ok((
            info.total_supply,
            info.price,
            info.payment_token,
            info.property_registry,
            info.compliance_registry,
        ))
    }

    /// Sets the RentDistributor contract address for yield checkpointing. Admin-only.
    pub fn set_rent_distributor(env: Env, distributor: Address) -> Result<(), FractionVaultError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(FractionVaultError::Unauthorized)?;
        admin.require_auth();
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "rent_distributor"), &distributor);
        Ok(())
    }

    /// Initiates a two-step admin transfer. The current admin nominates a new admin
    /// address, which must call `accept_admin()` to complete the handover.
    pub fn propose_admin(
        env: Env,
        new_admin: Address,
    ) -> Result<(), FractionVaultError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(FractionVaultError::Unauthorized)?;
        admin.require_auth();

        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin.clone());

        env.events()
            .publish((Symbol::new(&env, "AdminTransferProposed"),), (admin, new_admin));
        Ok(())
    }

    /// Completes the two-step admin transfer. The pending admin must call this.
    pub fn accept_admin(env: Env) -> Result<(), FractionVaultError> {
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .ok_or(FractionVaultError::NoPendingAdminTransfer)?;
        pending.require_auth();

        env.storage().instance().set(&DataKey::Admin, &pending);
        env.storage().instance().remove(&DataKey::PendingAdmin);

        env.events()
            .publish((Symbol::new(&env, "AdminTransferred"),), pending);
        Ok(())
    }

    fn checkpoint_yield(env: &Env, investor: &Address, prop_id: u64, balance: u128) {
        if let Some(distributor) = env
            .storage()
            .instance()
            .get::<Symbol, Address>(&Symbol::new(env, "rent_distributor"))
        {
            env.invoke_contract::<()>(
                &distributor,
                &Symbol::new(env, "checkpoint"),
                Vec::from_array(
                    env,
                    [
                        env.current_contract_address().to_val(),
                        investor.to_val(),
                        prop_id.into_val(env),
                        balance.into_val(env),
                    ],
                ),
            );
        }
    }

    /// Purchases `amount` fractions of a property. Checks compliance and enforces total supply cap.
    pub fn buy_fraction(
        env: Env,
        buyer: Address,
        prop_id: u64,
        amount: u128,
    ) -> Result<(), FractionVaultError> {
        buyer.require_auth();

        if amount == 0 {
            return Err(FractionVaultError::InvalidAmount);
        }

        let info: FractionInfo = env
            .storage()
            .instance()
            .get(&DataKey::FractionInfo(prop_id))
            .ok_or(FractionVaultError::PropertyNotFractionalized)?;

        // Enforce total supply cap
        let minted: u128 = env
            .storage()
            .instance()
            .get(&DataKey::MintedSupply(prop_id))
            .unwrap_or(0);
        if minted.checked_add(amount).unwrap() > info.total_supply {
            return Err(FractionVaultError::SupplyExceeded);
        }

        let key = DataKey::Balance(buyer.clone(), prop_id);
        let balance: u128 = env.storage().instance().get(&key).unwrap_or(0);

        // Checkpoint before balance change
        Self::checkpoint_yield(&env, &buyer, prop_id, balance);

        let property: PropertyData = env.invoke_contract(
            &info.property_registry,
            &Symbol::new(&env, "get_property"),
            Vec::from_array(&env, [prop_id.into_val(&env)]),
        );

        // Block purchases on inactive or under-maintenance properties
        if property.status != PropertyStatus::Active {
            return Err(FractionVaultError::PropertyNotActive);
        }

        let jurisdiction: Symbol = env.invoke_contract(
            &info.property_registry,
            &Symbol::new(&env, "get_property_jurisdiction"),
            Vec::from_array(&env, [prop_id.into_val(&env)]),
        );

        let compliant: bool = env.invoke_contract(
            &info.compliance_registry,
            &Symbol::new(&env, "is_compliant"),
            Vec::from_array(&env, [buyer.to_val(), jurisdiction.to_val()]),
        );
        if !compliant {
            return Err(FractionVaultError::ComplianceCheckFailed);
        }

        let new_balance = balance.checked_add(amount).unwrap();
        env.storage().instance().set(&key, &new_balance);

        if balance == 0 {
            let holder_key = DataKey::IsHolder(buyer.clone(), prop_id);
            let is_holder: bool = env.storage().instance().get(&holder_key).unwrap_or(false);
            if !is_holder {
                env.storage().instance().set(&holder_key, &true);
                let count_key = DataKey::HolderCount(prop_id);
                let count: u32 = env.storage().instance().get(&count_key).unwrap_or(0);
                env.storage().instance().set(&count_key, &(count + 1));
            }
        }

        // Update minted supply
        env.storage()
            .instance()
            .set(&DataKey::MintedSupply(prop_id), &(minted + amount));

        let payment = (amount as i128).checked_mul(info.price).unwrap();
        let vault = env.current_contract_address();
        env.invoke_contract::<()>(
            &info.payment_token,
            &Symbol::new(&env, "transfer"),
            Vec::from_array(
                &env,
                [buyer.to_val(), vault.to_val(), payment.into_val(&env)],
            ),
        );

        env.events().publish(
            (Symbol::new(&env, "FractionPurchased"), prop_id),
            (buyer, amount, payment),
        );

        Ok(())
    }

    /// Sells `amount` fractions with a minimum price floor. Cross-calls RentDistributor checkpoint.
    pub fn sell_fraction(
        env: Env,
        seller: Address,
        prop_id: u64,
        amount: u128,
        min_price: i128,
    ) -> Result<(), FractionVaultError> {
        seller.require_auth();

        if amount == 0 {
            return Err(FractionVaultError::InvalidAmount);
        }

        let info: FractionInfo = env
            .storage()
            .instance()
            .get(&DataKey::FractionInfo(prop_id))
            .ok_or(FractionVaultError::PropertyNotFractionalized)?;

        let key = DataKey::Balance(seller.clone(), prop_id);
        let balance: u128 = env.storage().instance().get(&key).unwrap_or(0);

        // Checkpoint before balance change
        Self::checkpoint_yield(&env, &seller, prop_id, balance);

        if balance < amount {
            return Err(FractionVaultError::InsufficientBalance);
        }

        let payout = (amount as i128).checked_mul(info.price).unwrap();
        if payout < min_price {
            return Err(FractionVaultError::PriceTooLow);
        }

        let new_balance = balance.checked_sub(amount).unwrap();
        env.storage().instance().set(&key, &new_balance);

        if new_balance == 0 {
            let holder_key = DataKey::IsHolder(seller.clone(), prop_id);
            env.storage().instance().remove(&holder_key);
            let count_key = DataKey::HolderCount(prop_id);
            let count: u32 = env.storage().instance().get(&count_key).unwrap_or(0);
            if count > 0 {
                env.storage().instance().set(&count_key, &(count - 1));
            }
        }

        // Update minted supply (fractions returned to "pool")
        let minted: u128 = env
            .storage()
            .instance()
            .get(&DataKey::MintedSupply(prop_id))
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::MintedSupply(prop_id), &(minted - amount));

        let vault = env.current_contract_address();
        env.invoke_contract::<()>(
            &info.payment_token,
            &Symbol::new(&env, "transfer"),
            Vec::from_array(
                &env,
                [vault.to_val(), seller.to_val(), payout.into_val(&env)],
            ),
        );

        env.events().publish(
            (Symbol::new(&env, "FractionSold"), prop_id),
            (seller, amount, payout),
        );

        Ok(())
    }

    /// Transfers `amount` fractions directly from sender to recipient with compliance check.
    pub fn transfer_fraction(
        env: Env,
        from: Address,
        to: Address,
        prop_id: u64,
        amount: u128,
    ) -> Result<(), FractionVaultError> {
        from.require_auth();

        if amount == 0 {
            return Err(FractionVaultError::InvalidAmount);
        }

        let info: FractionInfo = env
            .storage()
            .instance()
            .get(&DataKey::FractionInfo(prop_id))
            .ok_or(FractionVaultError::PropertyNotFractionalized)?;

        let from_key = DataKey::Balance(from.clone(), prop_id);
        let from_balance: u128 = env.storage().instance().get(&from_key).unwrap_or(0);
        if from_balance < amount {
            return Err(FractionVaultError::InsufficientBalance);
        }

        // Compliance check for recipient
        let jurisdiction: Symbol = env.invoke_contract(
            &info.property_registry,
            &Symbol::new(&env, "get_property_jurisdiction"),
            Vec::from_array(&env, [prop_id.into_val(&env)]),
        );
        let compliant: bool = env.invoke_contract(
            &info.compliance_registry,
            &Symbol::new(&env, "is_compliant"),
            Vec::from_array(&env, [to.to_val(), jurisdiction.to_val()]),
        );
        if !compliant {
            return Err(FractionVaultError::ComplianceCheckFailed);
        }

        // Checkpoint both parties before balance changes
        Self::checkpoint_yield(&env, &from, prop_id, from_balance);
        let to_key = DataKey::Balance(to.clone(), prop_id);
        let to_balance: u128 = env.storage().instance().get(&to_key).unwrap_or(0);
        Self::checkpoint_yield(&env, &to, prop_id, to_balance);

        // Update sender
        let new_from_balance = from_balance - amount;
        env.storage().instance().set(&from_key, &new_from_balance);
        if new_from_balance == 0 {
            let holder_key = DataKey::IsHolder(from.clone(), prop_id);
            env.storage().instance().remove(&holder_key);
            let count_key = DataKey::HolderCount(prop_id);
            let count: u32 = env.storage().instance().get(&count_key).unwrap_or(0);
            if count > 0 {
                env.storage().instance().set(&count_key, &(count - 1));
            }
        }

        // Update recipient
        let new_to_balance = to_balance + amount;
        env.storage().instance().set(&to_key, &new_to_balance);
        if to_balance == 0 {
            let holder_key = DataKey::IsHolder(to.clone(), prop_id);
            let is_holder: bool = env.storage().instance().get(&holder_key).unwrap_or(false);
            if !is_holder {
                env.storage().instance().set(&holder_key, &true);
                let count_key = DataKey::HolderCount(prop_id);
                let count: u32 = env.storage().instance().get(&count_key).unwrap_or(0);
                env.storage().instance().set(&count_key, &(count + 1));
            }
        }

        env.events().publish(
            (Symbol::new(&env, "FractionTransferred"), prop_id),
            (from, to, amount),
        );

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use propfi_compliance_registry::ComplianceRegistry;
    use propfi_compliance_registry::ComplianceRegistryClient;
    use propfi_property_registry::PropertyRegistry;
    use propfi_property_registry::PropertyRegistryClient;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{symbol_short, BytesN, Env};

    fn setup_base() -> (Env, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let owner = Address::generate(&env);
        (env, admin, owner)
    }

    fn register_property(env: &Env, admin: &Address, owner: &Address) -> (u64, Address, Address) {
        let jurisdiction = symbol_short!("US");

        let compliance_id = env.register_contract(None, ComplianceRegistry);
        let compliance_client = ComplianceRegistryClient::new(env, &compliance_id);
        compliance_client.initialize(admin);

        let prop_reg_id = env.register_contract(None, PropertyRegistry);
        let prop_reg_client = PropertyRegistryClient::new(env, &prop_reg_id);
        prop_reg_client.initialize(admin);

        let doc_hash = BytesN::from_array(env, &[0u8; 32]);
        let prop_id =
            prop_reg_client.register_property(owner, &100_000i128, &doc_hash, &jurisdiction);

        (prop_id, prop_reg_id, compliance_id)
    }

    fn setup_vault(env: &Env, admin: &Address) -> FractionVaultClient<'static> {
        let vault_id = env.register_contract(None, FractionVault);
        let vault_client = FractionVaultClient::new(env, &vault_id);
        vault_client.initialize(admin);
        vault_client
    }

    #[test]
    fn test_initialize() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let contract_id = env.register_contract(None, FractionVault);
        let client = FractionVaultClient::new(&env, &contract_id);
        client.initialize(&admin);
    }

    #[test]
    fn test_double_initialize_returns_error() {
        let (env, admin, _owner) = setup_base();
        let vault = setup_vault(&env, &admin);
        let rogue = Address::generate(&env);
        let result = vault.try_initialize(&rogue);
        assert!(result.is_err());
    }

    #[test]
    fn test_fractionalize() {
        let (env, admin, owner) = setup_base();
        let (prop_id, prop_reg_id, compliance_id) = register_property(&env, &admin, &owner);
        let vault = setup_vault(&env, &admin);

        let token = Address::generate(&env);
        vault.fractionalize(
            &prop_id,
            &1000u128,
            &100i128,
            &token,
            &prop_reg_id,
            &compliance_id,
        );

        assert_eq!(vault.total_holders(&prop_id), 0);
        assert_eq!(vault.get_balance(&owner, &prop_id), 0);
        assert_eq!(vault.minted_supply(&prop_id), 0);
    }

    #[test]
    fn test_fractionalize_nonexistent_property_returns_error() {
        let (env, admin, owner) = setup_base();
        let (_, prop_reg_id, compliance_id) = register_property(&env, &admin, &owner);
        let vault = setup_vault(&env, &admin);

        let token = Address::generate(&env);
        let result = vault.try_fractionalize(
            &99,
            &1000u128,
            &100i128,
            &token,
            &prop_reg_id,
            &compliance_id,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_double_fractionalize_returns_error() {
        let (env, admin, owner) = setup_base();
        let (prop_id, prop_reg_id, compliance_id) = register_property(&env, &admin, &owner);
        let vault = setup_vault(&env, &admin);

        let token = Address::generate(&env);
        vault.fractionalize(
            &prop_id,
            &1000u128,
            &100i128,
            &token,
            &prop_reg_id,
            &compliance_id,
        );
        let result = vault.try_fractionalize(
            &prop_id,
            &500u128,
            &50i128,
            &token,
            &prop_reg_id,
            &compliance_id,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_fractionalize_zero_supply_returns_error() {
        let (env, admin, owner) = setup_base();
        let (prop_id, prop_reg_id, compliance_id) = register_property(&env, &admin, &owner);
        let vault = setup_vault(&env, &admin);

        let token = Address::generate(&env);
        let result = vault.try_fractionalize(
            &prop_id,
            &0u128,
            &100i128,
            &token,
            &prop_reg_id,
            &compliance_id,
        );
        assert!(result.is_err());
    }

    fn setup_token(env: &Env, admin: &Address) -> Address {
        env.register_stellar_asset_contract_v2(admin.clone())
            .address()
    }

    fn attest_buyer(env: &Env, compliance_id: &Address, buyer: &Address) {
        let compliance_client = ComplianceRegistryClient::new(env, compliance_id);
        let proof = soroban_sdk::Bytes::from_slice(env, b"valid_proof");
        let jurisdiction = symbol_short!("US");
        compliance_client.attest(buyer, &proof, &jurisdiction, &365u32);
    }

    #[test]
    fn test_buy_fraction() {
        let (env, admin, owner) = setup_base();
        let prop_id = 1u64;

        let token = setup_token(&env, &admin);
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        sac.mint(&admin, &1_000_000i128);

        let (prop_id_reg, prop_reg_id, compliance_id) = register_property(&env, &admin, &owner);
        assert_eq!(prop_id_reg, prop_id);

        let vault = setup_vault(&env, &admin);
        vault.fractionalize(
            &prop_id,
            &1000u128,
            &100i128,
            &token,
            &prop_reg_id,
            &compliance_id,
        );

        let buyer = Address::generate(&env);
        sac.mint(&buyer, &100_000i128);
        attest_buyer(&env, &compliance_id, &buyer);

        vault.buy_fraction(&buyer, &prop_id, &10u128);

        assert_eq!(vault.get_balance(&buyer, &prop_id), 10);
        assert_eq!(vault.total_holders(&prop_id), 1);
        assert_eq!(vault.minted_supply(&prop_id), 10);
    }

    #[test]
    fn test_buy_fraction_supply_exceeded_returns_error() {
        let (env, admin, owner) = setup_base();
        let prop_id = 1u64;

        let token = setup_token(&env, &admin);
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        sac.mint(&admin, &1_000_000i128);

        let (prop_id_reg, prop_reg_id, compliance_id) = register_property(&env, &admin, &owner);
        assert_eq!(prop_id_reg, prop_id);

        let vault = setup_vault(&env, &admin);
        // Only 5 fractions total
        vault.fractionalize(
            &prop_id,
            &5u128,
            &100i128,
            &token,
            &prop_reg_id,
            &compliance_id,
        );

        let buyer = Address::generate(&env);
        sac.mint(&buyer, &100_000i128);
        attest_buyer(&env, &compliance_id, &buyer);

        // Attempting to buy 10 when only 5 exist should fail
        let result = vault.try_buy_fraction(&buyer, &prop_id, &10u128);
        assert!(result.is_err());
    }

    #[test]
    fn test_sell_fraction() {
        let (env, admin, owner) = setup_base();
        let prop_id = 1u64;

        let token = setup_token(&env, &admin);
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        sac.mint(&admin, &1_000_000i128);

        let (prop_id_reg, prop_reg_id, compliance_id) = register_property(&env, &admin, &owner);
        assert_eq!(prop_id_reg, prop_id);

        let vault = setup_vault(&env, &admin);
        vault.fractionalize(
            &prop_id,
            &1000u128,
            &100i128,
            &token,
            &prop_reg_id,
            &compliance_id,
        );

        let seller = Address::generate(&env);
        sac.mint(&seller, &100_000i128);
        attest_buyer(&env, &compliance_id, &seller);
        vault.buy_fraction(&seller, &prop_id, &10u128);
        assert_eq!(vault.get_balance(&seller, &prop_id), 10);
        assert_eq!(vault.minted_supply(&prop_id), 10);

        vault.sell_fraction(&seller, &prop_id, &4u128, &0i128);

        assert_eq!(vault.get_balance(&seller, &prop_id), 6);
        // Sold fractions go back to supply
        assert_eq!(vault.minted_supply(&prop_id), 6);
        assert_eq!(vault.total_holders(&prop_id), 1);
    }

    #[test]
    fn test_transfer_fraction() {
        let (env, admin, owner) = setup_base();
        let prop_id = 1u64;

        let token = setup_token(&env, &admin);
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        sac.mint(&admin, &1_000_000i128);

        let (prop_id_reg, prop_reg_id, compliance_id) = register_property(&env, &admin, &owner);
        assert_eq!(prop_id_reg, prop_id);

        let vault = setup_vault(&env, &admin);
        vault.fractionalize(
            &prop_id,
            &1000u128,
            &100i128,
            &token,
            &prop_reg_id,
            &compliance_id,
        );

        let from = Address::generate(&env);
        let to = Address::generate(&env);
        sac.mint(&from, &100_000i128);
        attest_buyer(&env, &compliance_id, &from);
        attest_buyer(&env, &compliance_id, &to);

        vault.buy_fraction(&from, &prop_id, &10u128);
        assert_eq!(vault.get_balance(&from, &prop_id), 10);
        assert_eq!(vault.total_holders(&prop_id), 1);

        vault.transfer_fraction(&from, &to, &prop_id, &5u128);

        assert_eq!(vault.get_balance(&from, &prop_id), 5);
        assert_eq!(vault.get_balance(&to, &prop_id), 5);
        assert_eq!(vault.total_holders(&prop_id), 2);
        // Minted supply unchanged by transfer
        assert_eq!(vault.minted_supply(&prop_id), 10);
    }

    #[test]
    fn test_transfer_fraction_non_compliant_returns_error() {
        let (env, admin, owner) = setup_base();
        let prop_id = 1u64;

        let token = setup_token(&env, &admin);
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        sac.mint(&admin, &1_000_000i128);

        let (prop_id_reg, prop_reg_id, compliance_id) = register_property(&env, &admin, &owner);
        assert_eq!(prop_id_reg, prop_id);

        let vault = setup_vault(&env, &admin);
        vault.fractionalize(
            &prop_id,
            &1000u128,
            &100i128,
            &token,
            &prop_reg_id,
            &compliance_id,
        );

        let from = Address::generate(&env);
        let to = Address::generate(&env); // not attested
        sac.mint(&from, &100_000i128);
        attest_buyer(&env, &compliance_id, &from);

        vault.buy_fraction(&from, &prop_id, &10u128);

        let result = vault.try_transfer_fraction(&from, &to, &prop_id, &5u128);
        assert!(result.is_err());
    }

    #[test]
    fn test_sell_fraction_insufficient_balance_returns_error() {
        let (env, admin, owner) = setup_base();
        let (prop_id, prop_reg_id, compliance_id) = register_property(&env, &admin, &owner);
        let vault = setup_vault(&env, &admin);
        let token = setup_token(&env, &admin);
        vault.fractionalize(
            &prop_id,
            &1000u128,
            &100i128,
            &token,
            &prop_reg_id,
            &compliance_id,
        );

        let seller = Address::generate(&env);
        let result = vault.try_sell_fraction(&seller, &prop_id, &1u128, &0i128);
        assert!(result.is_err());
    }

    #[test]
    fn test_buy_sell_full_lifecycle() {
        let (env, admin, owner) = setup_base();
        let prop_id = 1u64;

        let token = setup_token(&env, &admin);
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        sac.mint(&admin, &10_000_000i128);

        let (prop_id_reg, prop_reg_id, compliance_id) = register_property(&env, &admin, &owner);
        assert_eq!(prop_id_reg, prop_id);

        let vault = setup_vault(&env, &admin);
        vault.fractionalize(
            &prop_id,
            &1000u128,
            &100i128,
            &token,
            &prop_reg_id,
            &compliance_id,
        );

        let user = Address::generate(&env);
        sac.mint(&user, &1_000_000i128);
        attest_buyer(&env, &compliance_id, &user);

        vault.buy_fraction(&user, &prop_id, &50u128);
        assert_eq!(vault.get_balance(&user, &prop_id), 50);
        assert_eq!(vault.minted_supply(&prop_id), 50);

        vault.sell_fraction(&user, &prop_id, &20u128, &0i128);
        assert_eq!(vault.get_balance(&user, &prop_id), 30);
        assert_eq!(vault.minted_supply(&prop_id), 30);

        vault.buy_fraction(&user, &prop_id, &10u128);
        assert_eq!(vault.get_balance(&user, &prop_id), 40);
        assert_eq!(vault.minted_supply(&prop_id), 40);

        vault.sell_fraction(&user, &prop_id, &40u128, &0i128);
        assert_eq!(vault.get_balance(&user, &prop_id), 0);
        assert_eq!(vault.total_holders(&prop_id), 0);
        assert_eq!(vault.minted_supply(&prop_id), 0);
    }

    #[test]
    fn test_buy_fraction_inactive_property_returns_error() {
        use propfi_property_registry::PropertyRegistryClient;

        let (env, admin, owner) = setup_base();
        let prop_id = 1u64;

        let token = setup_token(&env, &admin);
        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token);
        sac.mint(&admin, &1_000_000i128);

        let (prop_id_reg, prop_reg_id, compliance_id) = register_property(&env, &admin, &owner);
        assert_eq!(prop_id_reg, prop_id);

        let vault = setup_vault(&env, &admin);
        vault.fractionalize(
            &prop_id,
            &1000u128,
            &100i128,
            &token,
            &prop_reg_id,
            &compliance_id,
        );

        // Mark the property as Inactive
        let prop_reg_client = PropertyRegistryClient::new(&env, &prop_reg_id);
        prop_reg_client.set_status(&prop_id, &propfi_types::PropertyStatus::Inactive);

        let buyer = Address::generate(&env);
        sac.mint(&buyer, &100_000i128);
        attest_buyer(&env, &compliance_id, &buyer);

        // Purchase should be blocked
        let result = vault.try_buy_fraction(&buyer, &prop_id, &10u128);
        assert!(result.is_err());

        // Re-activating should unblock purchases
        prop_reg_client.set_status(&prop_id, &propfi_types::PropertyStatus::Active);
        vault.buy_fraction(&buyer, &prop_id, &10u128);
        assert_eq!(vault.get_balance(&buyer, &prop_id), 10);
    }

    #[test]
    fn test_two_step_admin_transfer() {
        let (env, admin, _owner) = setup_base();
        let vault = setup_vault(&env, &admin);
        let new_admin = Address::generate(&env);

        // Step 1: current admin proposes
        vault.propose_admin(&new_admin);

        // Step 2: new admin accepts
        vault.accept_admin();

        // New admin can now fractionalize (admin-gated action)
        let (prop_id, prop_reg_id, compliance_id) = register_property(&env, &admin, &_owner);
        let token = Address::generate(&env);
        vault.fractionalize(
            &prop_id,
            &500u128,
            &10i128,
            &token,
            &prop_reg_id,
            &compliance_id,
        );
        assert_eq!(vault.minted_supply(&prop_id), 0);
    }

    #[test]
    fn test_accept_admin_without_proposal_returns_error() {
        let (env, admin, _owner) = setup_base();
        let vault = setup_vault(&env, &admin);
        let result = vault.try_accept_admin();
        assert!(result.is_err());
    }
}
