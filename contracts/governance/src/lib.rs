#![no_std]
//! On-chain protocol governance with proposal lifecycle. Fraction holders vote proportionally to their holdings. Features timelock-enforced execution.
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Bytes, Env, IntoVal, String,
    Symbol, Vec,
};

const VOTING_PERIOD: u64 = 48 * 3600;
const TIMELOCK_PERIOD: u64 = 24 * 3600;

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum GovernanceError {
    AlreadyInitialized = 1,
    Unauthorized = 2,
    ProposalNotFound = 3,
    ProposalAlreadyExecuted = 4,
    VotingPeriodEnded = 5,
    AlreadyVoted = 6,
    NoVotingPower = 7,
    VotingPeriodNotEnded = 8,
    TimelockNotElapsed = 9,
    QuorumNotMet = 10,
    ProposalDefeated = 11,
}

#[derive(Clone, Debug, PartialEq)]
#[contracttype]
pub struct ProposalData {
    pub proposer: Address,
    pub action_type: u32,
    pub calldata: Bytes,
    pub description: String,
    pub created_at: u64,
    pub voting_end: u64,
    pub executed: bool,
    pub for_votes: u128,
    pub against_votes: u128,
    pub quorum: u128,
}

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    Admin,
    FractionVault,
    ProposalCounter,
    Proposal(u64),
    HasVoted(u64, Address),
    TrackedProperties,
    Quorum,
}

/// Bump instance TTL to ~60 days (in ledgers at 5s/ledger) whenever the current
/// TTL falls below ~30 days. Called on every public entry point so active
/// contracts never silently expire their on-chain state.
const INSTANCE_TTL_THRESHOLD: u32 = 518_400;  // 30 days in ledgers
const INSTANCE_TTL_EXTEND_TO: u32 = 1_036_800; // 60 days in ledgers

#[contract]
pub struct Governance;

#[contractimpl]
impl Governance {
    /// Sets admin and FractionVault address. Called once at deployment.
    pub fn initialize(
        env: Env,
        admin: Address,
        fraction_vault: Address,
    ) -> Result<(), GovernanceError> {
        env.storage().instance().extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
        let existing: Option<Address> = env.storage().instance().get(&DataKey::Admin);
        if existing.is_some() {
            return Err(GovernanceError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::FractionVault, &fraction_vault);
        env.storage()
            .instance()
            .set(&DataKey::ProposalCounter, &0u64);
        env.storage().instance().set(&DataKey::Quorum, &0u128);
        env.storage()
            .instance()
            .set(&DataKey::TrackedProperties, &Vec::<u64>::new(&env));
        Ok(())
    }

    /// Updates the quorum required for proposals to pass. Admin-only.
    pub fn set_quorum(env: Env, quorum: u128) -> Result<(), GovernanceError> {
        env.storage().instance().extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(GovernanceError::Unauthorized)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Quorum, &quorum);
        Ok(())
    }

    /// Adds a property to the tracked set for voting power computation. Admin-only.
    pub fn add_tracked_property(env: Env, prop_id: u64) -> Result<(), GovernanceError> {
        env.storage().instance().extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(GovernanceError::Unauthorized)?;
        admin.require_auth();

        let mut props: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::TrackedProperties)
            .unwrap_or(Vec::new(&env));

        let mut exists = false;
        for i in 0..props.len() {
            if props.get(i).unwrap() == prop_id {
                exists = true;
                break;
            }
        }
        if !exists {
            props.push_back(prop_id);
            env.storage()
                .instance()
                .set(&DataKey::TrackedProperties, &props);
        }
        Ok(())
    }

    /// Creates a new proposal. The proposer must authorize and hold at least one fraction.
    pub fn propose(
        env: Env,
        proposer: Address,
        action_type: u32,
        calldata: Bytes,
        description: String,
    ) -> Result<u64, GovernanceError> {
        env.storage().instance().extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
        proposer.require_auth();

        // Only fraction holders may create proposals
        let power = Governance::voting_power_internal(&env, proposer.clone());
        if power == 0 {
            return Err(GovernanceError::NoVotingPower);
        }

        let mut counter: u64 = env
            .storage()
            .instance()
            .get(&DataKey::ProposalCounter)
            .unwrap_or(0);
        counter += 1;
        env.storage()
            .instance()
            .set(&DataKey::ProposalCounter, &counter);

        let now = env.ledger().timestamp();
        let quorum: u128 = env.storage().instance().get(&DataKey::Quorum).unwrap_or(0);

        let proposal = ProposalData {
            proposer: proposer.clone(),
            action_type,
            calldata,
            description,
            created_at: now,
            voting_end: now + VOTING_PERIOD,
            executed: false,
            for_votes: 0,
            against_votes: 0,
            quorum,
        };

        env.storage()
            .instance()
            .set(&DataKey::Proposal(counter), &proposal);

        env.events().publish(
            (Symbol::new(&env, "ProposalCreated"), counter),
            (proposer, action_type, now + VOTING_PERIOD),
        );

        Ok(counter)
    }

    /// Casts a vote (for/against) on a proposal. Voter must hold fractions.
    pub fn vote(
        env: Env,
        voter: Address,
        proposal_id: u64,
        support: bool,
    ) -> Result<(), GovernanceError> {
        env.storage().instance().extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
        voter.require_auth();

        let mut proposal: ProposalData = env
            .storage()
            .instance()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;

        if proposal.executed {
            return Err(GovernanceError::ProposalAlreadyExecuted);
        }

        let now = env.ledger().timestamp();
        if now > proposal.voting_end {
            return Err(GovernanceError::VotingPeriodEnded);
        }

        let voted_key = DataKey::HasVoted(proposal_id, voter.clone());
        if env.storage().instance().has(&voted_key) {
            return Err(GovernanceError::AlreadyVoted);
        }

        let power = Governance::voting_power_internal(&env, voter.clone());
        if power == 0 {
            return Err(GovernanceError::NoVotingPower);
        }

        env.storage().instance().set(&voted_key, &true);

        if support {
            proposal.for_votes = proposal.for_votes.checked_add(power).unwrap();
        } else {
            proposal.against_votes = proposal.against_votes.checked_add(power).unwrap();
        }

        env.storage()
            .instance()
            .set(&DataKey::Proposal(proposal_id), &proposal);

        env.events().publish(
            (Symbol::new(&env, "Voted"), proposal_id),
            (voter, support, power),
        );

        Ok(())
    }

    /// Executes a passed proposal after voting and timelock periods have elapsed.
    pub fn execute(env: Env, proposal_id: u64) -> Result<(), GovernanceError> {
        env.storage().instance().extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
        let mut proposal: ProposalData = env
            .storage()
            .instance()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;

        if proposal.executed {
            return Err(GovernanceError::ProposalAlreadyExecuted);
        }

        let now = env.ledger().timestamp();
        if now <= proposal.voting_end {
            return Err(GovernanceError::VotingPeriodNotEnded);
        }

        let earliest_execution = proposal.voting_end + TIMELOCK_PERIOD;
        if now < earliest_execution {
            return Err(GovernanceError::TimelockNotElapsed);
        }

        let total_votes = proposal.for_votes + proposal.against_votes;
        if total_votes < proposal.quorum {
            return Err(GovernanceError::QuorumNotMet);
        }

        if proposal.for_votes <= proposal.against_votes {
            return Err(GovernanceError::ProposalDefeated);
        }

        proposal.executed = true;
        env.storage()
            .instance()
            .set(&DataKey::Proposal(proposal_id), &proposal);

        env.events()
            .publish((Symbol::new(&env, "ProposalExecuted"), proposal_id), ());

        Ok(())
    }

    /// Returns the total voting power of a user based on their fraction holdings.
    pub fn voting_power(env: Env, user: Address) -> u128 {
        env.storage().instance().extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
        Governance::voting_power_internal(&env, user)
    }

    fn voting_power_internal(env: &Env, user: Address) -> u128 {
        let fraction_vault: Address = match env.storage().instance().get(&DataKey::FractionVault) {
            Some(v) => v,
            None => return 0,
        };

        let props: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::TrackedProperties)
            .unwrap_or(Vec::new(env));

        let mut total: u128 = 0;
        for i in 0..props.len() {
            let prop_id = props.get(i).unwrap();
            let balance: u128 = env.invoke_contract(
                &fraction_vault,
                &Symbol::new(env, "get_balance"),
                Vec::from_array(env, [user.to_val(), prop_id.into_val(env)]),
            );
            total = total.checked_add(balance).unwrap();
        }
        total
    }

    /// Returns the ProposalData for a given proposal ID.
    pub fn get_proposal(env: Env, proposal_id: u64) -> Result<ProposalData, GovernanceError> {
        env.storage().instance().extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
        env.storage()
            .instance()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use propfi_compliance_registry::ComplianceRegistry;
    use propfi_compliance_registry::ComplianceRegistryClient;
    use propfi_fraction_vault::FractionVault;
    use propfi_fraction_vault::FractionVaultClient;
    use propfi_property_registry::PropertyRegistry;
    use propfi_property_registry::PropertyRegistryClient;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::testutils::Ledger;
    use soroban_sdk::{symbol_short, BytesN, Env};

    fn setup_fraction_vault(env: &Env, admin: &Address) -> (Address, u64) {
        let property_owner = Address::generate(env);
        let jurisdiction = symbol_short!("US");

        let compliance_id = env.register_contract(None, ComplianceRegistry);
        let compliance_client = ComplianceRegistryClient::new(env, &compliance_id);
        compliance_client.initialize(admin);

        let prop_reg_id = env.register_contract(None, PropertyRegistry);
        let prop_reg_client = PropertyRegistryClient::new(env, &prop_reg_id);
        prop_reg_client.initialize(admin);

        let doc_hash = BytesN::from_array(env, &[0u8; 32]);
        let prop_id = prop_reg_client.register_property(
            &property_owner,
            &100_000i128,
            &doc_hash,
            &jurisdiction,
        );

        let vault_id = env.register_contract(None, FractionVault);
        let vault_client = FractionVaultClient::new(env, &vault_id);
        vault_client.initialize(admin);

        let token = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        vault_client.fractionalize(
            &prop_id,
            &1000u128,
            &100i128,
            &token,
            &prop_reg_id,
            &compliance_id,
        );

        (vault_id, prop_id)
    }

    fn give_voting_power(
        env: &Env,
        vault_client: &FractionVaultClient,
        vault_id: &Address,
        prop_id: u64,
        user: &Address,
        amount: u128,
    ) {
        let info = vault_client.get_fraction_info(&prop_id);
        let compliance_client = ComplianceRegistryClient::new(env, &info.4);
        let proof = soroban_sdk::Bytes::from_slice(env, b"proof");
        compliance_client.attest(user, &proof, &symbol_short!("US"), &365u32);
        let sac = soroban_sdk::token::StellarAssetClient::new(env, &info.2);
        sac.mint(user, &(amount as i128 * info.1));
        vault_client.buy_fraction(user, &prop_id, &amount);
        let _ = vault_id; // kept for future use
    }

    fn setup() -> (
        Env,
        Address,
        Address,
        GovernanceClient<'static>,
        Address,
        u64,
    ) {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        let (vault_id, prop_id) = setup_fraction_vault(&env, &admin);

        let contract_id = env.register_contract(None, Governance);
        let client = GovernanceClient::new(&env, &contract_id);
        client.initialize(&admin, &vault_id);

        client.add_tracked_property(&prop_id);

        (env, admin, user, client, vault_id, prop_id)
    }

    #[test]
    fn test_initialize() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let vault = Address::generate(&env);

        let contract_id = env.register_contract(None, Governance);
        let client = GovernanceClient::new(&env, &contract_id);
        client.initialize(&admin, &vault);
    }

    #[test]
    fn test_double_initialize_returns_error() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let vault = Address::generate(&env);

        let contract_id = env.register_contract(None, Governance);
        let client = GovernanceClient::new(&env, &contract_id);
        client.initialize(&admin, &vault);

        let rogue = Address::generate(&env);
        let result = client.try_initialize(&rogue, &vault);
        assert!(result.is_err());
    }

    #[test]
    fn test_propose() {
        let (env, _admin, user, client, vault_id, prop_id) = setup();
        let calldata = Bytes::from_array(&env, &[1, 2, 3]);
        let description = String::from_str(&env, "Test proposal");

        // Give user voting power first
        let vault_client = FractionVaultClient::new(&env, &vault_id);
        let info = vault_client.get_fraction_info(&prop_id);
        ComplianceRegistryClient::new(&env, &info.4).attest(
            &user,
            &Bytes::from_array(&env, &[]),
            &symbol_short!("US"),
            &365u32,
        );
        soroban_sdk::token::StellarAssetClient::new(&env, &info.2).mint(&user, &100_000i128);
        vault_client.buy_fraction(&user, &prop_id, &10u128);

        let pid = client.propose(&user, &1u32, &calldata, &description);
        assert_eq!(pid, 1);

        let proposal = client.get_proposal(&pid);
        assert_eq!(proposal.action_type, 1);
        assert_eq!(proposal.proposer, user);
        assert!(!proposal.executed);
        assert_eq!(proposal.for_votes, 0);
        assert_eq!(proposal.against_votes, 0);
    }

    #[test]
    fn test_vote_for() {
        let (env, _admin, user, client, vault_id, prop_id) = setup();

        let vault_client = FractionVaultClient::new(&env, &vault_id);
        give_voting_power(&env, &vault_client, &vault_id, prop_id, &user, 100);

        let calldata = Bytes::from_array(&env, &[]);
        let description = String::from_str(&env, "Test");
        let proposal_id = client.propose(&user, &1u32, &calldata, &description);

        client.vote(&user, &proposal_id, &true);

        let proposal = client.get_proposal(&proposal_id);
        assert_eq!(proposal.for_votes, 100);
        assert_eq!(proposal.against_votes, 0);
    }

    #[test]
    fn test_vote_against() {
        let (env, _admin, user, client, vault_id, prop_id) = setup();

        let vault_client = FractionVaultClient::new(&env, &vault_id);
        give_voting_power(&env, &vault_client, &vault_id, prop_id, &user, 50);

        let calldata = Bytes::from_array(&env, &[]);
        let description = String::from_str(&env, "Test");
        let proposal_id = client.propose(&user, &1u32, &calldata, &description);

        client.vote(&user, &proposal_id, &false);

        let proposal = client.get_proposal(&proposal_id);
        assert_eq!(proposal.for_votes, 0);
        assert_eq!(proposal.against_votes, 50);
    }

    #[test]
    fn test_double_vote_returns_error() {
        let (env, _admin, user, client, vault_id, prop_id) = setup();

        let vault_client = FractionVaultClient::new(&env, &vault_id);
        give_voting_power(&env, &vault_client, &vault_id, prop_id, &user, 100);

        let calldata = Bytes::from_array(&env, &[]);
        let description = String::from_str(&env, "Test");
        let proposal_id = client.propose(&user, &1u32, &calldata, &description);

        client.vote(&user, &proposal_id, &true);
        let result = client.try_vote(&user, &proposal_id, &false);
        assert!(result.is_err());
    }

    #[test]
    fn test_vote_after_deadline_returns_error() {
        let (env, _admin, user, client, vault_id, prop_id) = setup();

        let vault_client = FractionVaultClient::new(&env, &vault_id);
        give_voting_power(&env, &vault_client, &vault_id, prop_id, &user, 10);

        let calldata = Bytes::from_array(&env, &[]);
        let description = String::from_str(&env, "Test");
        let proposal_id = client.propose(&user, &1u32, &calldata, &description);

        env.ledger()
            .set_timestamp(env.ledger().timestamp() + VOTING_PERIOD + 1);

        let other = Address::generate(&env);
        let result = client.try_vote(&other, &proposal_id, &true);
        assert!(result.is_err());
    }

    #[test]
    fn test_execute_after_timelock() {
        let (env, _admin, user, client, vault_id, prop_id) = setup();

        let vault_client = FractionVaultClient::new(&env, &vault_id);
        give_voting_power(&env, &vault_client, &vault_id, prop_id, &user, 100);

        let calldata = Bytes::from_array(&env, &[]);
        let description = String::from_str(&env, "Test execution");
        let proposal_id = client.propose(&user, &1u32, &calldata, &description);

        client.vote(&user, &proposal_id, &true);

        env.ledger()
            .set_timestamp(env.ledger().timestamp() + VOTING_PERIOD + TIMELOCK_PERIOD + 1);

        client.execute(&proposal_id);

        let proposal = client.get_proposal(&proposal_id);
        assert!(proposal.executed);
    }

    #[test]
    fn test_execute_before_voting_ends_returns_error() {
        let (env, _admin, user, client, vault_id, prop_id) = setup();

        let vault_client = FractionVaultClient::new(&env, &vault_id);
        give_voting_power(&env, &vault_client, &vault_id, prop_id, &user, 100);

        let calldata = Bytes::from_array(&env, &[]);
        let description = String::from_str(&env, "Test");
        let proposal_id = client.propose(&user, &1u32, &calldata, &description);

        client.vote(&user, &proposal_id, &true);
        let result = client.try_execute(&proposal_id);
        assert!(result.is_err());
    }

    #[test]
    fn test_vote_without_power_returns_error() {
        let (env, _admin, user, client, vault_id, prop_id) = setup();

        // proposer needs power; user (the proposer) gets it
        let vault_client = FractionVaultClient::new(&env, &vault_id);
        give_voting_power(&env, &vault_client, &vault_id, prop_id, &user, 10);

        let calldata = Bytes::from_array(&env, &[]);
        let description = String::from_str(&env, "Test");
        let proposal_id = client.propose(&user, &1u32, &calldata, &description);

        // powerless voter should be rejected
        let no_power = Address::generate(&env);
        let result = client.try_vote(&no_power, &proposal_id, &true);
        assert!(result.is_err());
    }

    #[test]
    fn test_propose_without_power_returns_error() {
        let (env, _admin, user, client, _vault_id, _prop_id) = setup();

        let calldata = Bytes::from_array(&env, &[]);
        let description = String::from_str(&env, "Test");
        // user has no fractions → should fail
        let result = client.try_propose(&user, &1u32, &calldata, &description);
        assert!(result.is_err());
    }

    #[test]
    fn test_voting_power() {
        let (env, admin, user, _client, vault_id, prop_id) = setup();

        let vault_client = FractionVaultClient::new(&env, &vault_id);
        let info = vault_client.get_fraction_info(&prop_id);
        let compliance_client = ComplianceRegistryClient::new(&env, &info.4);
        let proof = soroban_sdk::Bytes::from_slice(&env, b"proof");
        compliance_client.attest(&user, &proof, &symbol_short!("US"), &365u32);

        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &info.2);
        sac.mint(&user, &100_000i128);
        vault_client.buy_fraction(&user, &prop_id, &75u128);

        let contract_id = env.register_contract(None, Governance);
        let client = GovernanceClient::new(&env, &contract_id);
        client.initialize(&admin, &vault_id);
        client.add_tracked_property(&prop_id);

        let power = client.voting_power(&user);
        assert_eq!(power, 75);
    }

    #[test]
    fn test_full_proposal_lifecycle() {
        let (env, _admin, user, client, vault_id, prop_id) = setup();

        let vault_client = FractionVaultClient::new(&env, &vault_id);
        give_voting_power(&env, &vault_client, &vault_id, prop_id, &user, 200);

        let calldata = Bytes::from_array(&env, &[0x01, 0x02]);
        let description = String::from_str(&env, "Update LTV parameter");
        let proposal_id = client.propose(&user, &1u32, &calldata, &description);

        client.vote(&user, &proposal_id, &true);
        let proposal = client.get_proposal(&proposal_id);
        assert_eq!(proposal.for_votes, 200);

        env.ledger()
            .set_timestamp(env.ledger().timestamp() + VOTING_PERIOD + TIMELOCK_PERIOD + 1);

        client.execute(&proposal_id);
        let proposal = client.get_proposal(&proposal_id);
        assert!(proposal.executed);
    }
}
