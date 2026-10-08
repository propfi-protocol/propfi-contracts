#![no_std]
//! Tokenizes real-world properties as on-chain assets with oracle-fed valuations and compliance-gated transfers.
use propfi_types::{PriceData, PropertyData, PropertyStatus};
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, BytesN, Env, Symbol, Vec,
};

const MAX_VALUATION_DEVIATION_BPS: i128 = 2000;

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum PropertyRegistryError {
    AlreadyInitialized = 1,
    Unauthorized = 2,
    PropertyNotFound = 3,
    InvalidValuation = 4,
    ComplianceCheckFailed = 5,
    OraclePriceNotAvailable = 6,
    ValuationOutOfRange = 7,
}

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    Admin,
    PropertyCounter,
    Property(u64),
    Jurisdiction(u64),
}

#[contract]
pub struct PropertyRegistry;

#[contractimpl]
impl PropertyRegistry {
    /// Sets the admin address. Called once at deployment.
    pub fn initialize(env: Env, admin: Address) -> Result<(), PropertyRegistryError> {
        let existing: Option<Address> = env.storage().instance().get(&DataKey::Admin);
        if existing.is_some() {
            return Err(PropertyRegistryError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        Ok(())
    }

    /// Registers a new property with owner, valuation, doc hash, and jurisdiction. Admin-only. Returns the new property ID.
    pub fn register_property(
        env: Env,
        owner: Address,
        valuation: i128,
        doc_hash: BytesN<32>,
        jurisdiction: Symbol,
    ) -> Result<u64, PropertyRegistryError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(PropertyRegistryError::Unauthorized)?;
        admin.require_auth();

        if valuation <= 0 {
            return Err(PropertyRegistryError::InvalidValuation);
        }

        let mut counter: u64 = env
            .storage()
            .instance()
            .get(&DataKey::PropertyCounter)
            .unwrap_or(0);
        counter += 1;
        env.storage()
            .instance()
            .set(&DataKey::PropertyCounter, &counter);

        let now = env.ledger().timestamp();
        let property = PropertyData {
            owner: owner.clone(),
            valuation,
            doc_hash,
            status: PropertyStatus::Active,
            created_at: now,
            updated_at: now,
        };

        env.storage()
            .instance()
            .set(&DataKey::Property(counter), &property);
        env.storage()
            .instance()
            .set(&DataKey::Jurisdiction(counter), &jurisdiction);

        env.events().publish(
            (Symbol::new(&env, "PropertyRegistered"), counter),
            (owner, valuation, jurisdiction),
        );

        Ok(counter)
    }

    /// Updates a property's valuation using an oracle price feed. Only callable by the property owner.
    pub fn update_valuation(
        env: Env,
        prop_id: u64,
        new_val: i128,
        oracle_contract: Address,
        asset: Symbol,
    ) -> Result<(), PropertyRegistryError> {
        let mut property: PropertyData = env
            .storage()
            .instance()
            .get(&DataKey::Property(prop_id))
            .ok_or(PropertyRegistryError::PropertyNotFound)?;

        property.owner.require_auth();

        if new_val <= 0 {
            return Err(PropertyRegistryError::InvalidValuation);
        }

        let price_data: PriceData = env.invoke_contract(
            &oracle_contract,
            &Symbol::new(&env, "get_price"),
            Vec::from_array(&env, [asset.to_val()]),
        );

        if price_data.price <= 0 {
            return Err(PropertyRegistryError::OraclePriceNotAvailable);
        }

        let deviation = if new_val > price_data.price {
            new_val - price_data.price
        } else {
            price_data.price - new_val
        };

        let max_deviation = price_data.price * MAX_VALUATION_DEVIATION_BPS / 10000;
        if deviation > max_deviation {
            return Err(PropertyRegistryError::ValuationOutOfRange);
        }

        property.valuation = new_val;
        property.updated_at = env.ledger().timestamp();
        env.storage()
            .instance()
            .set(&DataKey::Property(prop_id), &property);

        env.events()
            .publish((Symbol::new(&env, "ValuationUpdated"), prop_id), new_val);
        Ok(())
    }

    /// Transfers property ownership to a new address. Checks compliance via the provided ComplianceRegistry contract.
    pub fn transfer_ownership(
        env: Env,
        prop_id: u64,
        to: Address,
        compliance_contract: Address,
    ) -> Result<(), PropertyRegistryError> {
        let mut property: PropertyData = env
            .storage()
            .instance()
            .get(&DataKey::Property(prop_id))
            .ok_or(PropertyRegistryError::PropertyNotFound)?;

        property.owner.require_auth();

        let jurisdiction: Symbol = env
            .storage()
            .instance()
            .get(&DataKey::Jurisdiction(prop_id))
            .ok_or(PropertyRegistryError::PropertyNotFound)?;

        let compliant: bool = env.invoke_contract(
            &compliance_contract,
            &Symbol::new(&env, "is_compliant"),
            Vec::from_array(&env, [to.to_val(), jurisdiction.to_val()]),
        );

        if !compliant {
            return Err(PropertyRegistryError::ComplianceCheckFailed);
        }

        let from = property.owner.clone();
        property.owner = to.clone();
        property.updated_at = env.ledger().timestamp();
        env.storage()
            .instance()
            .set(&DataKey::Property(prop_id), &property);

        env.events().publish(
            (Symbol::new(&env, "OwnershipTransferred"), prop_id),
            (from, to),
        );
        Ok(())
    }

    /// Returns the PropertyData for the given property ID.
    pub fn get_property(env: Env, prop_id: u64) -> Result<PropertyData, PropertyRegistryError> {
        env.storage()
            .instance()
            .get(&DataKey::Property(prop_id))
            .ok_or(PropertyRegistryError::PropertyNotFound)
    }

    /// Updates the property status (Active, Inactive, UnderMaintenance). Admin-only.
    pub fn set_status(
        env: Env,
        prop_id: u64,
        status: PropertyStatus,
    ) -> Result<(), PropertyRegistryError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(PropertyRegistryError::Unauthorized)?;
        admin.require_auth();

        let mut property: PropertyData = env
            .storage()
            .instance()
            .get(&DataKey::Property(prop_id))
            .ok_or(PropertyRegistryError::PropertyNotFound)?;

        property.status = status;
        property.updated_at = env.ledger().timestamp();
        env.storage()
            .instance()
            .set(&DataKey::Property(prop_id), &property);
        Ok(())
    }

    /// Returns the jurisdiction symbol for a property.
    pub fn get_property_jurisdiction(
        env: Env,
        prop_id: u64,
    ) -> Result<Symbol, PropertyRegistryError> {
        env.storage()
            .instance()
            .get(&DataKey::Jurisdiction(prop_id))
            .ok_or(PropertyRegistryError::PropertyNotFound)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use propfi_compliance_registry::ComplianceRegistry;
    use propfi_oracle_adapter::OracleAdapter;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{BytesN, Env};

    fn setup_property_registry() -> (Env, Address, PropertyRegistryClient<'static>) {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let contract_id = env.register_contract(None, PropertyRegistry);
        let client = PropertyRegistryClient::new(&env, &contract_id);

        client.initialize(&admin);

        (env, admin, client)
    }

    fn register_test_property(
        client: &PropertyRegistryClient<'static>,
        env: &Env,
        owner: &Address,
        valuation: i128,
        jurisdiction: &Symbol,
    ) -> u64 {
        let doc_hash = BytesN::from_array(env, &[0u8; 32]);
        client.register_property(owner, &valuation, &doc_hash, jurisdiction)
    }

    fn setup_compliance_registry(env: &Env, admin: &Address) -> Address {
        let contract_id = env.register_contract(None, ComplianceRegistry);
        let compliance_client =
            propfi_compliance_registry::ComplianceRegistryClient::new(env, &contract_id);
        compliance_client.initialize(admin);
        contract_id
    }

    fn setup_oracle_adapter(
        env: &Env,
        admin: &Address,
        oracle: &Address,
        asset: &Symbol,
        price: i128,
    ) -> Address {
        let contract_id = env.register_contract(None, OracleAdapter);
        let oracle_client = propfi_oracle_adapter::OracleAdapterClient::new(env, &contract_id);
        oracle_client.initialize(admin, &86400u64);
        oracle_client.add_oracle(oracle, &100u32);
        oracle_client.submit_price(oracle, asset, &price);
        contract_id
    }

    #[test]
    fn test_initialize() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let contract_id = env.register_contract(None, PropertyRegistry);
        let client = PropertyRegistryClient::new(&env, &contract_id);

        client.initialize(&admin);
    }

    #[test]
    fn test_double_initialize_returns_error() {
        let (env, _admin, client) = setup_property_registry();
        let rogue_admin = Address::generate(&env);
        let result = client.try_initialize(&rogue_admin);
        assert!(result.is_err());
    }

    #[test]
    fn test_register_property() {
        let (env, _admin, client) = setup_property_registry();
        let owner = Address::generate(&env);
        let valuation: i128 = 100_000;
        let jurisdiction = Symbol::new(&env, "US");
        let doc_hash = BytesN::from_array(&env, &[1u8; 32]);

        let prop_id = client.register_property(&owner, &valuation, &doc_hash, &jurisdiction);
        assert_eq!(prop_id, 1);

        let property = client.get_property(&prop_id);
        assert_eq!(property.owner, owner);
        assert_eq!(property.valuation, valuation);
        assert_eq!(property.doc_hash, doc_hash);
        assert_eq!(property.status, PropertyStatus::Active);
        assert_eq!(property.created_at, property.updated_at);

        let stored_jurisdiction = client.get_property_jurisdiction(&prop_id);
        assert_eq!(stored_jurisdiction, jurisdiction);
    }

    #[test]
    fn test_register_multiple_properties() {
        let (env, _admin, client) = setup_property_registry();
        let owner = Address::generate(&env);

        let id1 = register_test_property(&client, &env, &owner, 100_000, &Symbol::new(&env, "US"));
        let id2 = register_test_property(&client, &env, &owner, 200_000, &Symbol::new(&env, "EU"));
        let id3 = register_test_property(&client, &env, &owner, 300_000, &Symbol::new(&env, "NG"));

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);

        assert_eq!(client.get_property(&id1).valuation, 100_000);
        assert_eq!(client.get_property(&id2).valuation, 200_000);
        assert_eq!(client.get_property(&id3).valuation, 300_000);
    }

    #[test]
    fn test_register_property_zero_valuation_returns_error() {
        let (env, _admin, client) = setup_property_registry();
        let owner = Address::generate(&env);
        let doc_hash = BytesN::from_array(&env, &[0u8; 32]);
        let result = client.try_register_property(&owner, &0, &doc_hash, &Symbol::new(&env, "US"));
        assert!(result.is_err());
    }

    #[test]
    fn test_register_property_negative_valuation_returns_error() {
        let (env, _admin, client) = setup_property_registry();
        let owner = Address::generate(&env);
        let doc_hash = BytesN::from_array(&env, &[0u8; 32]);
        let result =
            client.try_register_property(&owner, &-1, &doc_hash, &Symbol::new(&env, "US"));
        assert!(result.is_err());
    }

    #[test]
    fn test_get_nonexistent_property_returns_error() {
        let (_env, _admin, client) = setup_property_registry();
        let result = client.try_get_property(&99);
        assert!(result.is_err());
    }

    #[test]
    fn test_set_status() {
        let (env, _admin, client) = setup_property_registry();
        let owner = Address::generate(&env);
        let prop_id =
            register_test_property(&client, &env, &owner, 100_000, &Symbol::new(&env, "US"));

        client.set_status(&prop_id, &PropertyStatus::UnderMaintenance);
        assert_eq!(
            client.get_property(&prop_id).status,
            PropertyStatus::UnderMaintenance
        );

        client.set_status(&prop_id, &PropertyStatus::Inactive);
        assert_eq!(
            client.get_property(&prop_id).status,
            PropertyStatus::Inactive
        );

        client.set_status(&prop_id, &PropertyStatus::Active);
        assert_eq!(client.get_property(&prop_id).status, PropertyStatus::Active);
    }

    #[test]
    fn test_set_status_nonexistent_property_returns_error() {
        let (_env, _admin, client) = setup_property_registry();
        let result = client.try_set_status(&99, &PropertyStatus::Inactive);
        assert!(result.is_err());
    }

    #[test]
    fn test_transfer_ownership() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let owner = Address::generate(&env);
        let recipient = Address::generate(&env);
        let jurisdiction = Symbol::new(&env, "US");

        // Setup compliance registry with recipient attested
        let compliance_id = setup_compliance_registry(&env, &admin);
        let compliance_client =
            propfi_compliance_registry::ComplianceRegistryClient::new(&env, &compliance_id);
        let proof = soroban_sdk::Bytes::from_slice(&env, b"valid_proof");
        compliance_client.attest(&recipient, &proof, &jurisdiction, &365u32);

        // Setup property registry
        let prop_reg_id = env.register_contract(None, PropertyRegistry);
        let client = PropertyRegistryClient::new(&env, &prop_reg_id);
        client.initialize(&admin);

        let prop_id = register_test_property(&client, &env, &owner, 100_000, &jurisdiction);

        client.transfer_ownership(&prop_id, &recipient, &compliance_id);

        let property = client.get_property(&prop_id);
        assert_eq!(property.owner, recipient);
    }

    #[test]
    fn test_transfer_ownership_non_compliant_recipient_returns_error() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let owner = Address::generate(&env);
        let recipient = Address::generate(&env);
        let jurisdiction = Symbol::new(&env, "US");

        let compliance_id = setup_compliance_registry(&env, &admin);

        let prop_reg_id = env.register_contract(None, PropertyRegistry);
        let client = PropertyRegistryClient::new(&env, &prop_reg_id);
        client.initialize(&admin);

        let prop_id = register_test_property(&client, &env, &owner, 100_000, &jurisdiction);

        let result = client.try_transfer_ownership(&prop_id, &recipient, &compliance_id);
        assert!(result.is_err());
    }

    #[test]
    fn test_transfer_ownership_wrong_jurisdiction_returns_error() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let owner = Address::generate(&env);
        let recipient = Address::generate(&env);

        let compliance_id = setup_compliance_registry(&env, &admin);
        let compliance_client =
            propfi_compliance_registry::ComplianceRegistryClient::new(&env, &compliance_id);
        let proof = soroban_sdk::Bytes::from_slice(&env, b"valid_proof");
        compliance_client.attest(&recipient, &proof, &Symbol::new(&env, "EU"), &365u32);

        let prop_reg_id = env.register_contract(None, PropertyRegistry);
        let client = PropertyRegistryClient::new(&env, &prop_reg_id);
        client.initialize(&admin);

        let prop_id =
            register_test_property(&client, &env, &owner, 100_000, &Symbol::new(&env, "US"));

        let result = client.try_transfer_ownership(&prop_id, &recipient, &compliance_id);
        assert!(result.is_err());
    }

    #[test]
    fn test_transfer_nonexistent_property_returns_error() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);

        let compliance_id = setup_compliance_registry(&env, &admin);

        let prop_reg_id = env.register_contract(None, PropertyRegistry);
        let client = PropertyRegistryClient::new(&env, &prop_reg_id);
        client.initialize(&admin);

        let result = client.try_transfer_ownership(&99, &recipient, &compliance_id);
        assert!(result.is_err());
    }

    #[test]
    fn test_update_valuation() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let owner = Address::generate(&env);
        let oracle = Address::generate(&env);
        let asset = Symbol::new(&env, "PROP_USD");

        let oracle_id = setup_oracle_adapter(&env, &admin, &oracle, &asset, 100_000);

        let prop_reg_id = env.register_contract(None, PropertyRegistry);
        let client = PropertyRegistryClient::new(&env, &prop_reg_id);
        client.initialize(&admin);

        let prop_id =
            register_test_property(&client, &env, &owner, 90_000, &Symbol::new(&env, "US"));

        client.update_valuation(&prop_id, &110_000, &oracle_id, &asset);

        let property = client.get_property(&prop_id);
        assert_eq!(property.valuation, 110_000);
    }

    #[test]
    fn test_update_valuation_out_of_range_returns_error() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let owner = Address::generate(&env);
        let oracle = Address::generate(&env);
        let asset = Symbol::new(&env, "PROP_USD");

        let oracle_id = setup_oracle_adapter(&env, &admin, &oracle, &asset, 100_000);

        let prop_reg_id = env.register_contract(None, PropertyRegistry);
        let client = PropertyRegistryClient::new(&env, &prop_reg_id);
        client.initialize(&admin);

        let prop_id =
            register_test_property(&client, &env, &owner, 90_000, &Symbol::new(&env, "US"));

        let result = client.try_update_valuation(&prop_id, &130_000, &oracle_id, &asset);
        assert!(result.is_err());
    }

    #[test]
    fn test_full_property_lifecycle() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let owner = Address::generate(&env);
        let buyer = Address::generate(&env);
        let oracle = Address::generate(&env);
        let asset = Symbol::new(&env, "PROP_USD");
        let jurisdiction = Symbol::new(&env, "US");

        let compliance_id = setup_compliance_registry(&env, &admin);
        let compliance_client =
            propfi_compliance_registry::ComplianceRegistryClient::new(&env, &compliance_id);
        let proof = soroban_sdk::Bytes::from_slice(&env, b"buyer_proof");
        compliance_client.attest(&buyer, &proof, &jurisdiction, &365u32);

        let oracle_id = setup_oracle_adapter(&env, &admin, &oracle, &asset, 100_000);

        let prop_reg_id = env.register_contract(None, PropertyRegistry);
        let client = PropertyRegistryClient::new(&env, &prop_reg_id);
        client.initialize(&admin);

        let prop_id = register_test_property(&client, &env, &owner, 90_000, &jurisdiction);
        assert_eq!(client.get_property(&prop_id).owner, owner);

        client.update_valuation(&prop_id, &105_000, &oracle_id, &asset);
        assert_eq!(client.get_property(&prop_id).valuation, 105_000);

        client.transfer_ownership(&prop_id, &buyer, &compliance_id);
        assert_eq!(client.get_property(&prop_id).owner, buyer);

        client.set_status(&prop_id, &PropertyStatus::UnderMaintenance);
        assert_eq!(
            client.get_property(&prop_id).status,
            PropertyStatus::UnderMaintenance
        );

        client.set_status(&prop_id, &PropertyStatus::Active);
        assert_eq!(client.get_property(&prop_id).status, PropertyStatus::Active);
    }
}
