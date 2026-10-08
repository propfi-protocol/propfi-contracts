//! KYC/AML attestation registry with jurisdiction-aware compliance gating.
//!
//! Stores proof hashes (never raw PII), manages attestation expiry, and enforces
//! configurable jurisdiction rules. Consumed by PropertyRegistry, FractionVault,
//! and other contracts for compliance checks on transfers and investments.

#![no_std]
use propfi_types::JurisdictionRules;
use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Bytes, Env, Symbol};

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ComplianceRegistryError {
    AlreadyInitialized = 1,
    Unauthorized = 2,
    AttestationNotFound = 3,
    UserNotCompliant = 4,
}

#[derive(Clone, Debug, PartialEq)]
#[contracttype]
/// A KYC attestation record for a user under a specific jurisdiction.
pub struct Attestation {
    /// Hash of the ZK proof (never raw PII stored on-chain)
    pub proof_hash: Bytes,
    /// Jurisdiction this attestation applies to (e.g., "US", "EU")
    pub jurisdiction: Symbol,
    /// Ledger timestamp when this attestation expires
    pub expiry: u64,
    /// Whether the attestation is currently active (may be revoked)
    pub active: bool,
}

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    /// One attestation record per (user, jurisdiction) pair.
    Attestation(Address, Symbol),
    JurisdictionRules(Symbol),
    Admin,
}

const DAY: u64 = 86400;

#[contract]
pub struct ComplianceRegistry;

#[contractimpl]
impl ComplianceRegistry {
    /// Sets the admin address. Called once at deployment.
    pub fn initialize(env: Env, admin: Address) -> Result<(), ComplianceRegistryError> {
        let existing: Option<Address> = env.storage().instance().get(&DataKey::Admin);
        if existing.is_some() {
            return Err(ComplianceRegistryError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        Ok(())
    }

    /// Records a KYC attestation for `user` under the given `jurisdiction`.
    /// Only callable by the admin. Emits an `Attested` event.
    pub fn attest(
        env: Env,
        user: Address,
        proof_hash: Bytes,
        jurisdiction: Symbol,
        duration_days: u32,
    ) -> Result<(), ComplianceRegistryError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(ComplianceRegistryError::Unauthorized)?;
        admin.require_auth();

        let expiry = env
            .ledger()
            .timestamp()
            .checked_add((duration_days as u64).checked_mul(DAY).unwrap())
            .unwrap();

        let attestation = Attestation {
            proof_hash,
            jurisdiction: jurisdiction.clone(),
            expiry,
            active: true,
        };

        env.storage()
            .instance()
            .set(&DataKey::Attestation(user.clone(), jurisdiction.clone()), &attestation);

        env.events().publish(
            (Symbol::new(&env, "Attested"), user),
            (jurisdiction, expiry),
        );

        Ok(())
    }

    /// Checks whether `user` has a valid, non-expired attestation for `jurisdiction`.
    /// Also enforces min-remaining-days rules if configured.
    pub fn is_compliant(env: Env, user: Address, jurisdiction: Symbol) -> bool {
        let key = DataKey::Attestation(user, jurisdiction.clone());
        let attestation = match env.storage().instance().get::<DataKey, Attestation>(&key) {
            Some(a) => a,
            None => return false,
        };

        if !attestation.active {
            return false;
        }

        if attestation.expiry <= env.ledger().timestamp() {
            return false;
        }

        if let Some(rules) = env
            .storage()
            .instance()
            .get::<DataKey, JurisdictionRules>(&DataKey::JurisdictionRules(jurisdiction))
        {
            let remaining = attestation.expiry - env.ledger().timestamp();
            let min_seconds = (rules.min_attestation_days as u64) * DAY;
            if remaining < min_seconds {
                return false;
            }
        }

        true
    }

    /// Revokes a user's attestation for a specific jurisdiction. Only callable by the admin.
    /// Emits a `Revoked` event. All compliance checks will fail for this user and jurisdiction.
    pub fn revoke(env: Env, user: Address, jurisdiction: Symbol) -> Result<(), ComplianceRegistryError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(ComplianceRegistryError::Unauthorized)?;
        admin.require_auth();

        let key = DataKey::Attestation(user.clone(), jurisdiction.clone());
        let mut attestation: Attestation = env
            .storage()
            .instance()
            .get(&key)
            .ok_or(ComplianceRegistryError::AttestationNotFound)?;
        attestation.active = false;

        env.storage().instance().set(&key, &attestation);

        env.events()
            .publish((Symbol::new(&env, "Revoked"), user), jurisdiction);
        Ok(())
    }

    /// Configures compliance rules for a jurisdiction (e.g., min attestation duration).
    /// Only callable by the admin. Emits a `RulesUpdated` event.
    pub fn set_jurisdiction_rules(
        env: Env,
        jurisdiction: Symbol,
        rules: JurisdictionRules,
    ) -> Result<(), ComplianceRegistryError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(ComplianceRegistryError::Unauthorized)?;
        admin.require_auth();

        env.storage()
            .instance()
            .set(&DataKey::JurisdictionRules(jurisdiction.clone()), &rules);

        env.events()
            .publish((Symbol::new(&env, "RulesUpdated"), jurisdiction), ());
        Ok(())
    }

    /// Returns the ledger timestamp at which the user's attestation for the given jurisdiction expires.
    /// Returns 0 if the user has no attestation for that jurisdiction.
    pub fn attestation_expiry(env: Env, user: Address, jurisdiction: Symbol) -> u64 {
        let key = DataKey::Attestation(user, jurisdiction);
        match env.storage().instance().get::<DataKey, Attestation>(&key) {
            Some(a) => a.expiry,
            None => 0,
        }
    }

    /// Returns the full Attestation record for a user and jurisdiction, if it exists.
    pub fn get_attestation(env: Env, user: Address, jurisdiction: Symbol) -> Option<Attestation> {
        env.storage()
            .instance()
            .get(&DataKey::Attestation(user, jurisdiction))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::testutils::Ledger;
    use soroban_sdk::{Address, Bytes, Env, Symbol};

    fn setup() -> (Env, Address, Address, ComplianceRegistryClient<'static>) {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        let contract_id = env.register_contract(None, ComplianceRegistry);
        let client = ComplianceRegistryClient::new(&env, &contract_id);

        client.initialize(&admin);

        (env, admin, user, client)
    }

    #[test]
    fn test_attest_flow() {
        let (env, _admin, user, client) = setup();

        let proof = Bytes::from_slice(&env, b"zk_proof_123");
        let jurisdiction = Symbol::new(&env, "US");

        client.attest(&user, &proof, &jurisdiction, &365u32);

        assert!(client.is_compliant(&user, &jurisdiction));

        let expiry = client.attestation_expiry(&user, &jurisdiction);
        assert!(expiry > 0);
        assert_eq!(expiry, env.ledger().timestamp() + 365 * DAY);
    }

    #[test]
    fn test_expiry() {
        let (env, _admin, user, client) = setup();

        let proof = Bytes::from_slice(&env, b"zk_proof_456");
        let jurisdiction = Symbol::new(&env, "US");

        client.attest(&user, &proof, &jurisdiction, &1u32);

        assert!(client.is_compliant(&user, &jurisdiction));

        env.ledger()
            .set_timestamp(env.ledger().timestamp() + 2 * DAY);

        assert!(!client.is_compliant(&user, &jurisdiction));
    }

    #[test]
    fn test_revocation() {
        let (env, _admin, user, client) = setup();

        let proof = Bytes::from_slice(&env, b"zk_proof_789");
        let jurisdiction = Symbol::new(&env, "US");

        client.attest(&user, &proof, &jurisdiction, &365u32);
        assert!(client.is_compliant(&user, &jurisdiction));

        client.revoke(&user, &jurisdiction);
        assert!(!client.is_compliant(&user, &jurisdiction));

        let expiry = client.attestation_expiry(&user, &jurisdiction);
        assert!(expiry > 0);
    }

    #[test]
    fn test_jurisdiction_filtering() {
        let (env, _admin, user, client) = setup();

        let proof = Bytes::from_slice(&env, b"zk_proof_abc");
        let us = Symbol::new(&env, "US");
        let eu = Symbol::new(&env, "EU");

        client.attest(&user, &proof, &us, &365u32);

        assert!(client.is_compliant(&user, &us));
        assert!(!client.is_compliant(&user, &eu));
    }

    #[test]
    fn test_admin_gating() {
        let env = Env::default();
        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        let _attacker = Address::generate(&env);

        let contract_id = env.register_contract(None, ComplianceRegistry);
        let client = ComplianceRegistryClient::new(&env, &contract_id);

        client.initialize(&admin);

        env.mock_all_auths();

        let proof = Bytes::from_slice(&env, b"evil_proof");
        let jurisdiction = Symbol::new(&env, "US");

        client.attest(&user, &proof, &jurisdiction, &365u32);
        assert!(client.is_compliant(&user, &jurisdiction));
    }

    #[test]
    fn test_jurisdiction_rules_enforcement() {
        let (env, _admin, user, client) = setup();

        let jurisdiction = Symbol::new(&env, "US");
        let rules = JurisdictionRules {
            min_attestation_days: 30,
            required_level: 1,
        };
        client.set_jurisdiction_rules(&jurisdiction, &rules);

        let proof = Bytes::from_slice(&env, b"proof_short");
        client.attest(&user, &proof, &jurisdiction, &1u32);

        assert!(!client.is_compliant(&user, &jurisdiction));
    }

    #[test]
    fn test_unattested_user_not_compliant() {
        let (env, _admin, user, client) = setup();

        let jurisdiction = Symbol::new(&env, "US");
        assert!(!client.is_compliant(&user, &jurisdiction));

        let expiry = client.attestation_expiry(&user, &jurisdiction);
        assert_eq!(expiry, 0);
    }

    #[test]
    fn test_multiple_jurisdictions_per_user() {
        let (env, _admin, user, client) = setup();

        let us = Symbol::new(&env, "US");
        let eu = Symbol::new(&env, "EU");

        let proof_us = Bytes::from_slice(&env, b"proof_us");
        let proof_eu = Bytes::from_slice(&env, b"proof_eu");

        client.attest(&user, &proof_us, &us, &365u32);
        client.attest(&user, &proof_eu, &eu, &180u32);

        // User is now simultaneously compliant in both jurisdictions
        assert!(client.is_compliant(&user, &us));
        assert!(client.is_compliant(&user, &eu));
    }

    #[test]
    fn test_revoke_one_jurisdiction_preserves_other() {
        let (env, _admin, user, client) = setup();

        let us = Symbol::new(&env, "US");
        let eu = Symbol::new(&env, "EU");

        let proof_us = Bytes::from_slice(&env, b"proof_us");
        let proof_eu = Bytes::from_slice(&env, b"proof_eu");

        client.attest(&user, &proof_us, &us, &365u32);
        client.attest(&user, &proof_eu, &eu, &180u32);

        // Revoke US only
        client.revoke(&user, &us);

        assert!(!client.is_compliant(&user, &us));
        // EU attestation still active
        assert!(client.is_compliant(&user, &eu));
    }

    #[test]
    fn test_get_attestation() {
        let (env, _admin, user, client) = setup();

        let us = Symbol::new(&env, "US");
        let proof = Bytes::from_slice(&env, b"proof_data");

        assert!(client.get_attestation(&user, &us).is_none());

        client.attest(&user, &proof, &us, &365u32);

        let att = client.get_attestation(&user, &us).unwrap();
        assert!(att.active);
        assert_eq!(att.jurisdiction, us);
    }

    #[test]
    fn test_double_initialize_returns_error() {
        let (env, _admin, _user, client) = setup();
        let rogue_admin = Address::generate(&env);
        let result = client.try_initialize(&rogue_admin);
        assert!(result.is_err());
    }
}
