use crate::auth::AuthError;
use soroban_sdk::{
    contracterror, contracttype, panic_with_error, Address, Env, IntoVal, Symbol, Vec,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum GovernanceError {
    NotInitialized = 1,
    Unauthorized = 2,
    AlreadyVoted = 3,
    ProposalNotFound = 4,
    ThresholdNotMet = 5,
    TimelockNotExpired = 6,
    AlreadyExecuted = 7,
    InvalidProposalPayload = 8,
    // Issue #189: proposer does not meet the minimum token balance.
    InsufficientStake = 9,
    // Issue #188: security council veto helpers.
    NotCouncil = 10,
    AlreadyVetoed = 11,
    ProposalVetoed = 12,
    // Delegated voting (liquid democracy).
    SelfDelegation = 13,
    DelegationChainNotAllowed = 14,
    NotDelegated = 15,
    Overflow = 16,
}

/// Minimal threshold-based governance.
///
/// Security model:
/// - Owners set at initialization
/// - Any governance action is represented by a `proposal_key` (Symbol)
/// - Owners create a proposal (or any one can create; approvals still required)
/// - Approvers call `approve(proposal_id)`
/// - Once `threshold` unique approvals are collected, the action can be executed
/// - A designated security council can veto a proposal (M-of-N vote) before it
///   executes; vetoed proposals can never execute (Issue #188)
///
/// NOTE: This is intentionally generic and uses a `proposal_payload_hash` to bind params.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum ProposalType {
    Normal = 1,
    Emergency = 2,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum ProposalCategory {
    General = 1,
    ParameterChange = 2,
    Emergency = 3,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct QuorumConfig {
    pub min_participation_bps: u32,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum VotingModel {
    Linear = 1,
    Quadratic = 2,
    Delegated = 3,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VotePowerInfo {
    pub raw_votes: i128,
    pub effective_votes: i128,
    pub voting_model: VotingModel,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProposalSimulationResult {
    pub affected_contracts: soroban_sdk::Vec<Symbol>,
    pub state_changes: u32,
    pub estimated_gas: u64,
    pub requires_migration: bool,
}

#[contracttype]
#[derive(Clone)]
pub struct Proposal {
    pub id: u64,
    pub proposal_key: Symbol,
    pub category: ProposalCategory,
    pub payload_hash: soroban_sdk::BytesN<32>,
    pub created_at: u64,
    pub review_until: u64,
    pub executable_after: u64,
    pub approvals: soroban_sdk::Vec<Address>,
    pub executed: bool,
    pub vetoed: bool,
    pub veto_votes: soroban_sdk::Vec<Address>,
    pub proposal_type: ProposalType,
    pub voting_model: VotingModel,
}

pub fn integer_sqrt(n: i128) -> i128 {
    if n <= 0 {
        return 0;
    }
    let mut x0 = n / 2;
    if x0 == 0 {
        return 1;
    }
    let mut x1 = (x0 + n / x0) / 2;
    while x1 < x0 {
        x0 = x1;
        x1 = (x0 + n / x0) / 2;
    }
    x0
}

pub fn calculate_voting_power(balance: i128, model: VotingModel) -> i128 {
    match model {
        VotingModel::Linear | VotingModel::Delegated => balance,
        VotingModel::Quadratic => integer_sqrt(balance),
    }
}

fn get_category_review_period(env: &Env, category: &ProposalCategory) -> u64 {
    match category {
        ProposalCategory::General => 86400,
        ProposalCategory::ParameterChange => 172800,
        ProposalCategory::Emergency => 3600,
    }
}

fn read_quorum_config(env: &Env) -> QuorumConfig {
    env.storage()
        .instance()
        .get(&Symbol::new(env, "gov_quorum_config"))
        .unwrap_or(QuorumConfig {
            min_participation_bps: 2000,
        })
}

fn quorum_bps(env: &Env, votes_cast: u32) -> u32 {
    let owners = read_owners(env);
    if owners.len() == 0 {
        return 0;
    }
    (votes_cast * 10000) / owners.len()
}

pub fn write_governance(
    env: &Env,
    auth: &Address,
    owners: &Vec<Address>,
    threshold: u32,
    timelock_seconds: u64,
) {
    auth.require_auth();

    // one-time init
    let init_key = Symbol::new(env, "gov_initialized");
    if env.storage().instance().has(&init_key) {
        panic_with_error!(env, AuthError::AlreadyInitialized);
    }

    let owners_key = Symbol::new(env, "gov_owners");
    let threshold_key = Symbol::new(env, "gov_threshold");
    let timelock_key = Symbol::new(env, "gov_timelock_seconds");

    env.storage().instance().set(&owners_key, owners);
    env.storage().instance().set(&threshold_key, &threshold);
    env.storage()
        .instance()
        .set(&timelock_key, &timelock_seconds);
    env.storage().instance().set(&init_key, &true);

    // default quorum (20%) for normal proposals
    let default_quorum = QuorumConfig {
        min_participation_bps: 2000,
    };
    env.storage()
        .instance()
        .set(&Symbol::new(env, "gov_quorum_config"), &default_quorum);

    // proposal id counter
    env.storage()
        .instance()
        .set(&Symbol::new(env, "gov_proposal_count"), &0u64);

    // Issue #189: spam protection defaults (no stake required until set).
    env.storage()
        .instance()
        .set(&Symbol::new(env, "gov_min_proposal_balance"), &0u128);
}

fn read_owners(env: &Env) -> Vec<Address> {
    env.storage()
        .instance()
        .get(&Symbol::new(env, "gov_owners"))
        .unwrap_or_else(|| panic_with_error!(env, AuthError::NotInitialized))
}

fn read_threshold(env: &Env) -> u32 {
    env.storage()
        .instance()
        .get(&Symbol::new(env, "gov_threshold"))
        .unwrap_or_else(|| panic_with_error!(env, AuthError::NotInitialized))
}

fn read_timelock_seconds(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get(&Symbol::new(env, "gov_timelock_seconds"))
        .unwrap_or(0u64)
}

fn read_min_proposal_balance(env: &Env) -> u128 {
    env.storage()
        .instance()
        .get(&Symbol::new(env, "gov_min_proposal_balance"))
        .unwrap_or(0u128)
}

fn read_stake_token(env: &Env) -> Address {
    env.storage()
        .instance()
        .get(&Symbol::new(env, "gov_stake_token"))
        .unwrap_or_else(|| panic_with_error!(env, GovernanceError::InvalidProposalPayload))
}

fn read_council(env: &Env) -> soroban_sdk::Vec<Address> {
    env.storage()
        .instance()
        .get(&Symbol::new(env, "gov_council"))
        .unwrap_or(soroban_sdk::Vec::new(env))
}

fn read_council_threshold(env: &Env) -> u32 {
    env.storage()
        .instance()
        .get(&Symbol::new(env, "gov_council_threshold"))
        .unwrap_or(1u32)
}

fn is_owner(env: &Env, addr: &Address) -> bool {
    let owners = read_owners(env);
    owners.iter().any(|o| o.clone() == *addr)
}

fn is_council_member(env: &Env, addr: &Address) -> bool {
    let council = read_council(env);
    council.iter().any(|c| c.clone() == *addr)
}

/// Issue #189: set the minimum token balance a proposer must hold (excluding
/// locked tokens) to create a proposal. A value of `0` disables the check.
pub fn set_min_proposal_balance(env: &Env, admin: Address, min: u128) {
    admin.require_auth();
    if !is_owner(env, &admin) {
        panic_with_error!(env, GovernanceError::Unauthorized);
    }

    env.storage()
        .instance()
        .set(&Symbol::new(env, "gov_min_proposal_balance"), &min);
}

/// Issue #189: configure the token whose unlocked balance is used to enforce
/// the minimum proposal stake.
pub fn set_stake_token(env: &Env, admin: Address, token: Address) {
    admin.require_auth();
    if !is_owner(env, &admin) {
        panic_with_error!(env, GovernanceError::Unauthorized);
    }

    env.storage()
        .instance()
        .set(&Symbol::new(env, "gov_stake_token"), &token);
}

/// Issue #188: (re)configure the security council. `council` is the full
/// member list and `veto_threshold` is M for the M-of-N veto. Emits a
/// `council_updated` event.
pub fn update_council(
    env: &Env,
    admin: Address,
    council: soroban_sdk::Vec<Address>,
    veto_threshold: u32,
) {
    admin.require_auth();
    if !is_owner(env, &admin) {
        panic_with_error!(env, GovernanceError::Unauthorized);
    }
    if veto_threshold == 0 || veto_threshold > council.len() {
        panic_with_error!(env, GovernanceError::InvalidProposalPayload);
    }

    let council_key = Symbol::new(env, "gov_council");
    let threshold_key = Symbol::new(env, "gov_council_threshold");
    env.storage().instance().set(&council_key, &council);
    env.storage().instance().set(&threshold_key, &veto_threshold);

    env.events().publish(
        (Symbol::new(env, "council_updated"), council.len(), veto_threshold),
        (),
    );
}

/// Compute the proposer's stake that counts toward the minimum: total balance
/// minus locked tokens.
fn proposer_unlocked_stake(env: &Env, token: &Address, proposer: &Address) -> i128 {
    let args: soroban_sdk::Vec<soroban_sdk::Val> =
        soroban_sdk::vec![env, proposer.clone().into_val(env)];
    let balance: crate::rwa_token::Balance =
        env.invoke_contract(token, &Symbol::new(env, "get_balance"), args);
    balance.amount.saturating_sub(balance.locked_amount).max(0)
}

pub fn create_proposal_with_model(
    env: Env,
    proposer: Address,
    proposal_key: Symbol,
    category: ProposalCategory,
    payload_hash: soroban_sdk::BytesN<32>,
    voting_model: VotingModel,
) -> u64 {
    proposer.require_auth();

    if !is_owner(&env, &proposer) {
        panic_with_error!(&env, GovernanceError::Unauthorized);
    }

    let min_balance = read_min_proposal_balance(&env);
    if min_balance > 0 {
        let stake_token = read_stake_token(&env);
        let unlocked = proposer_unlocked_stake(&env, &stake_token, &proposer);
        if (unlocked as u128) < min_balance {
            panic_with_error!(&env, GovernanceError::InsufficientStake);
        }
    }

    let proposal_count: u64 = env
        .storage()
        .instance()
        .get(&Symbol::new(&env, "gov_proposal_count"))
        .unwrap_or(0u64);

    let timelock = read_timelock_seconds(&env);
    let now = env.ledger().timestamp();
    let review_period = get_category_review_period(&env, &category);
    let review_until = now + review_period;
    let executable_after = review_until + timelock;

    let proposal = Proposal {
        id: proposal_count + 1,
        proposal_key,
        category,
        payload_hash,
        created_at: now,
        review_until,
        executable_after,
        approvals: soroban_sdk::Vec::<Address>::new(&env),
        executed: false,
        vetoed: false,
        veto_votes: soroban_sdk::Vec::<Address>::new(&env),
        proposal_type: ProposalType::Normal,
        voting_model,
    };

    let proposals_key = Symbol::new(&env, "gov_proposals");
    let mut proposals: soroban_sdk::Map<u64, Proposal> = env
        .storage()
        .instance()
        .get(&proposals_key)
        .unwrap_or(soroban_sdk::Map::new(&env));

    let id = proposal.id;
    proposals.set(id, proposal.clone());
    env.storage().instance().set(&proposals_key, &proposals);

    env.storage()
        .instance()
        .set(&Symbol::new(&env, "gov_proposal_count"), &id);

    env.events().publish(
        (
            Symbol::new(&env, "governance"),
            Symbol::new(&env, "proposal_created"),
            ProposalType::Normal,
        ),
        proposal,
    );

    id
}

pub fn create_proposal(
    env: Env,
    proposer: Address,
    proposal_key: Symbol,
    payload_hash: soroban_sdk::BytesN<32>,
) -> u64 {
    create_proposal_with_model(
        env,
        proposer,
        proposal_key,
        ProposalCategory::General,
        payload_hash,
        VotingModel::Linear,
    )
}

pub fn create_emergency_proposal(
    env: Env,
    proposer: Address,
    proposal_key: Symbol,
    payload_hash: soroban_sdk::BytesN<32>,
) -> u64 {
    proposer.require_auth();

    if !is_owner(&env, &proposer) {
        panic_with_error!(&env, GovernanceError::Unauthorized);
    }

    let proposal_count: u64 = env
        .storage()
        .instance()
        .get(&Symbol::new(&env, "gov_proposal_count"))
        .unwrap_or(0u64);

    let emergency_review_seconds: u64 = env
        .storage()
        .instance()
        .get(&Symbol::new(&env, "gov_emergency_review_seconds"))
        .unwrap_or(3600u64);

    let now = env.ledger().timestamp();
    let review_until = now + emergency_review_seconds;

    let proposal = Proposal {
        id: proposal_count + 1,
        proposal_key,
        category: ProposalCategory::Emergency,
        payload_hash,
        created_at: now,
        review_until,
        executable_after: review_until,
        approvals: soroban_sdk::Vec::<Address>::new(&env),
        executed: false,
        vetoed: false,
        veto_votes: soroban_sdk::Vec::<Address>::new(&env),
        proposal_type: ProposalType::Emergency,
        voting_model: VotingModel::Linear,
    };

    let proposals_key = Symbol::new(&env, "gov_proposals");
    let mut proposals: soroban_sdk::Map<u64, Proposal> = env
        .storage()
        .instance()
        .get(&proposals_key)
        .unwrap_or(soroban_sdk::Map::new(&env));

    let id = proposal.id;
    proposals.set(id, proposal.clone());
    env.storage().instance().set(&proposals_key, &proposals);

    env.storage()
        .instance()
        .set(&Symbol::new(&env, "gov_proposal_count"), &id);

    env.events().publish(
        (
            Symbol::new(&env, "governance"),
            Symbol::new(&env, "proposal_created"),
            ProposalType::Emergency,
            Symbol::new(&env, "EMERGENCY"),
        ),
        proposal,
    );

    id
}

/// Set the review window (in seconds) applied to emergency proposals.
pub fn set_emergency_review_seconds(env: &Env, caller: &Address, seconds: u64) {
    caller.require_auth();
    if !is_owner(env, caller) {
        panic_with_error!(env, GovernanceError::Unauthorized);
    }
    env.storage()
        .instance()
        .set(&Symbol::new(env, "gov_emergency_review_seconds"), &seconds);
}

pub fn approve(env: Env, approver: Address, proposal_id: u64) {
    approver.require_auth();

    if !is_owner(&env, &approver) {
        panic_with_error!(&env, GovernanceError::Unauthorized);
    }

    let proposals_key = Symbol::new(&env, "gov_proposals");
    let mut proposals: soroban_sdk::Map<u64, Proposal> = env
        .storage()
        .instance()
        .get(&proposals_key)
        .unwrap_or(soroban_sdk::Map::new(&env));

    let mut proposal = proposals
        .get(proposal_id)
        .unwrap_or_else(|| panic_with_error!(&env, GovernanceError::ProposalNotFound));

    if proposal.executed {
        panic_with_error!(&env, GovernanceError::AlreadyExecuted);
    }

    if proposal.approvals.iter().any(|a| a.clone() == approver) {
        panic_with_error!(&env, GovernanceError::AlreadyVoted);
    }

    proposal.approvals.push_back(approver);
    proposals.set(proposal_id, proposal);
    env.storage().instance().set(&proposals_key, &proposals);
}

/// Issue #188: record a veto vote from a security council member. A proposal
/// is vetoed once `veto_threshold` (M) unique council members have voted.
///
/// Returns the number of veto votes recorded on this proposal so far. Emits a
/// `proposal_vetoed` event when the threshold is reached.
pub fn veto_proposal(env: Env, council_member: Address, proposal_id: u64) -> u32 {
    council_member.require_auth();
    if !is_council_member(&env, &council_member) {
        panic_with_error!(&env, GovernanceError::NotCouncil);
    }

    let proposals_key = Symbol::new(&env, "gov_proposals");
    let mut proposals: soroban_sdk::Map<u64, Proposal> = env
        .storage()
        .instance()
        .get(&proposals_key)
        .unwrap_or(soroban_sdk::Map::new(&env));

    let mut proposal = proposals
        .get(proposal_id)
        .unwrap_or_else(|| panic_with_error!(&env, GovernanceError::ProposalNotFound));

    if proposal.executed {
        panic_with_error!(&env, GovernanceError::AlreadyExecuted);
    }
    if proposal.vetoed {
        panic_with_error!(&env, GovernanceError::AlreadyVetoed);
    }
    if proposal.veto_votes.iter().any(|v| v.clone() == council_member) {
        panic_with_error!(&env, GovernanceError::AlreadyVetoed);
    }

    proposal.veto_votes.push_back(council_member);
    let votes = proposal.veto_votes.len() as u32;

    let threshold = read_council_threshold(&env);
    if votes >= threshold {
        proposal.vetoed = true;
        env.events().publish(
            (Symbol::new(&env, "proposal_vetoed"), proposal_id),
            (),
        );
    }

    proposals.set(proposal_id, proposal);
    env.storage().instance().set(&proposals_key, &proposals);

    votes
}

pub fn read_proposal(env: &Env, proposal_id: u64) -> Proposal {
    let proposals_key = Symbol::new(env, "gov_proposals");
    let proposals: soroban_sdk::Map<u64, Proposal> = env
        .storage()
        .instance()
        .get(&proposals_key)
        .unwrap_or(soroban_sdk::Map::new(env));

    proposals
        .get(proposal_id)
        .unwrap_or_else(|| panic_with_error!(env, GovernanceError::ProposalNotFound))
}

/// Issue #188: whether the security council has vetoed the proposal.
pub fn proposal_is_vetoed(env: &Env, proposal_id: u64) -> bool {
    let proposal = read_proposal(env, proposal_id);
    proposal.vetoed
}

pub fn can_execute(
    env: &Env,
    proposal_id: u64,
    proposal_key: Symbol,
    payload_hash: soroban_sdk::BytesN<32>,
) -> bool {
    let proposals_key = Symbol::new(env, "gov_proposals");
    let proposals: soroban_sdk::Map<u64, Proposal> = env
        .storage()
        .instance()
        .get(&proposals_key)
        .unwrap_or(soroban_sdk::Map::new(env));

    let proposal = proposals.get(proposal_id);
    if proposal.is_none() {
        return false;
    }
    let proposal = proposal.unwrap();

    if proposal.executed {
        return false;
    }
    // Issue #188: vetoed proposals can never execute.
    if proposal.vetoed {
        return false;
    }
    if proposal.proposal_key != proposal_key {
        return false;
    }
    if proposal.payload_hash != payload_hash {
        return false;
    }

    let now = env.ledger().timestamp();
    if now < proposal.executable_after {
        return false;
    }

    match proposal.proposal_type {
        ProposalType::Normal => {
            let threshold = read_threshold(env);
            if proposal.approvals.len() < threshold {
                return false;
            }
            // Quorum must be satisfied for normal proposals.
            let quorum = read_quorum_config(env);
            let votes_cast = proposal.approvals.len();
            let q = quorum_bps(env, votes_cast);
            q >= quorum.min_participation_bps
        }
        ProposalType::Emergency => {
            // Emergency proposals require an 80% threshold and expire after 48h.
            let threshold_bps = 8000u32;
            let votes_cast = proposal.approvals.len();
            let participation = quorum_bps(env, votes_cast);
            if participation < threshold_bps {
                return false;
            }
            let expires_at = proposal.created_at.saturating_add(172800);
            now <= expires_at
        }
    }
}

pub fn execute_mark(env: Env, executor: Address, proposal_id: u64) {
    executor.require_auth();
    if !is_owner(&env, &executor) {
        panic_with_error!(&env, GovernanceError::Unauthorized);
    }

    let proposals_key = Symbol::new(&env, "gov_proposals");
    let mut proposals: soroban_sdk::Map<u64, Proposal> = env
        .storage()
        .instance()
        .get(&proposals_key)
        .unwrap_or(soroban_sdk::Map::new(&env));

    let mut proposal = proposals
        .get(proposal_id)
        .unwrap_or_else(|| panic_with_error!(&env, GovernanceError::ProposalNotFound));

    if proposal.executed {
        panic_with_error!(&env, GovernanceError::AlreadyExecuted);
    }

    // Issue #188: vetoed proposals must never execute.
    if proposal.vetoed {
        panic_with_error!(&env, GovernanceError::ProposalVetoed);
    }

    proposal.executed = true;
    proposals.set(proposal_id, proposal);
    env.storage().instance().set(&proposals_key, &proposals);
}

/// Read-only dry-run that estimates the impact of executing a proposal.
/// Any caller may use this; it performs no state changes.
pub fn simulate_proposal(env: &Env, proposal_id: u64) -> ProposalSimulationResult {
    let proposals_key = Symbol::new(env, "gov_proposals");
    let proposals: soroban_sdk::Map<u64, Proposal> = env
        .storage()
        .instance()
        .get(&proposals_key)
        .unwrap_or(soroban_sdk::Map::new(env));

    let proposal = proposals
        .get(proposal_id)
        .unwrap_or_else(|| panic_with_error!(env, GovernanceError::ProposalNotFound));

    if proposal.executed {
        panic_with_error!(env, GovernanceError::AlreadyExecuted);
    }

    // Deterministic estimates derived from the proposal's own parameters.
    let mut affected_contracts = Vec::new(env);
    affected_contracts.push_back(proposal.proposal_key);

    let state_changes = 1u32;
    let estimated_gas = 50_000u64 + (proposal.approvals.len() as u64) * 1_000u64;

    let payload_flag = proposal.payload_hash.first().unwrap_or(0u8);
    let requires_migration = payload_flag & 0x01 == 0x01;

    ProposalSimulationResult {
        affected_contracts,
        state_changes,
        estimated_gas,
        requires_migration,
    }
}

// ── Delegated voting (liquid democracy) ──────────────────────────────────────
//
// Token holders may delegate their voting power to a representative. The
// delegation graph lives in the *calling contract's* instance storage, so these
// helpers can be reused by any contract that embeds them (e.g. `RWAToken`).
//
// Semantics:
// - A holder keeps at most one active delegation; delegating again replaces it.
// - Delegated power = own voting power + power delegated to the address.
// - Max chain depth is 1: representatives may not re-delegate the power they
//   receive, so chains such as A -> B -> C are rejected at delegation time.
//
// Storage keys (instance):
//   "gov_delegations"       : Map<Address, Address>  owner -> delegate
//   "gov_delegated_power"   : Map<Address, i128>     delegate -> total power received
//   "gov_delegated_count"   : Map<Address, u32>      delegate -> # of delegators
//   "gov_delegated_amounts" : Map<Address, i128>     owner -> power currently delegated

pub fn delegate_votes(env: Env, owner: Address, delegate: Address, own_voting_power: i128) {
    owner.require_auth();

    if owner == delegate {
        panic_with_error!(&env, GovernanceError::SelfDelegation);
    }

    let delegations_key = Symbol::new(&env, "gov_delegations");
    let delegated_power_key = Symbol::new(&env, "gov_delegated_power");
    let delegated_count_key = Symbol::new(&env, "gov_delegated_count");
    let delegated_amounts_key = Symbol::new(&env, "gov_delegated_amounts");

    let mut delegations: soroban_sdk::Map<Address, Address> = env
        .storage()
        .instance()
        .get(&delegations_key)
        .unwrap_or(soroban_sdk::Map::new(&env));
    let mut delegated_power: soroban_sdk::Map<Address, i128> = env
        .storage()
        .instance()
        .get(&delegated_power_key)
        .unwrap_or(soroban_sdk::Map::new(&env));
    let mut delegated_count: soroban_sdk::Map<Address, u32> = env
        .storage()
        .instance()
        .get(&delegated_count_key)
        .unwrap_or(soroban_sdk::Map::new(&env));
    let mut delegated_amounts: soroban_sdk::Map<Address, i128> = env
        .storage()
        .instance()
        .get(&delegated_amounts_key)
        .unwrap_or(soroban_sdk::Map::new(&env));

    // Max chain depth 1: the target must not itself be a delegator, otherwise
    // the received power would flow further downstream (A -> B -> C).
    if delegations.contains_key(delegate.clone()) {
        panic_with_error!(&env, GovernanceError::DelegationChainNotAllowed);
    }
    // Max chain depth 1: an address that already receives delegated power may
    // not push that power further downstream.
    if delegated_count.get(owner.clone()).unwrap_or(0) > 0 {
        panic_with_error!(&env, GovernanceError::DelegationChainNotAllowed);
    }

    // Re-delegation replaces the previous delegation from this owner.
    if let Some(previous) = delegations.get(owner.clone()) {
        let prev_amount = delegated_amounts.get(owner.clone()).unwrap_or(0);
        let mut power = delegated_power.get(previous.clone()).unwrap_or(0);
        power = power.checked_sub(prev_amount).unwrap_or(0);
        if power > 0 {
            delegated_power.set(previous.clone(), power);
        } else {
            delegated_power.remove(previous.clone());
        }
        let mut count = delegated_count.get(previous.clone()).unwrap_or(0);
        count = count.saturating_sub(1);
        if count > 0 {
            delegated_count.set(previous.clone(), count);
        } else {
            delegated_count.remove(previous.clone());
        }
        delegated_amounts.remove(owner.clone());
    }

    delegations.set(owner.clone(), delegate.clone());
    delegated_amounts.set(owner.clone(), own_voting_power);
    let new_power = delegated_power
        .get(delegate.clone())
        .unwrap_or(0)
        .checked_add(own_voting_power)
        .unwrap_or_else(|| panic_with_error!(&env, GovernanceError::Overflow));
    delegated_power.set(delegate.clone(), new_power);
    delegated_count.set(
        delegate.clone(),
        delegated_count.get(delegate.clone()).unwrap_or(0) + 1,
    );

    env.storage().instance().set(&delegations_key, &delegations);
    env.storage().instance().set(&delegated_power_key, &delegated_power);
    env.storage().instance().set(&delegated_count_key, &delegated_count);
    env.storage().instance().set(&delegated_amounts_key, &delegated_amounts);

    env.events().publish(
        (Symbol::new(&env, "votes_delegated"), owner),
        (delegate, own_voting_power),
    );
}

pub fn undelegate_votes(env: Env, owner: Address) {
    owner.require_auth();

    let delegations_key = Symbol::new(&env, "gov_delegations");
    let delegated_power_key = Symbol::new(&env, "gov_delegated_power");
    let delegated_count_key = Symbol::new(&env, "gov_delegated_count");
    let delegated_amounts_key = Symbol::new(&env, "gov_delegated_amounts");

    let mut delegations: soroban_sdk::Map<Address, Address> = env
        .storage()
        .instance()
        .get(&delegations_key)
        .unwrap_or(soroban_sdk::Map::new(&env));
    let mut delegated_power: soroban_sdk::Map<Address, i128> = env
        .storage()
        .instance()
        .get(&delegated_power_key)
        .unwrap_or(soroban_sdk::Map::new(&env));
    let mut delegated_count: soroban_sdk::Map<Address, u32> = env
        .storage()
        .instance()
        .get(&delegated_count_key)
        .unwrap_or(soroban_sdk::Map::new(&env));
    let mut delegated_amounts: soroban_sdk::Map<Address, i128> = env
        .storage()
        .instance()
        .get(&delegated_amounts_key)
        .unwrap_or(soroban_sdk::Map::new(&env));

    let delegate = delegations
        .get(owner.clone())
        .unwrap_or_else(|| panic_with_error!(&env, GovernanceError::NotDelegated));
    let prev_amount = delegated_amounts.get(owner.clone()).unwrap_or(0);

    delegations.remove(owner.clone());
    delegated_amounts.remove(owner.clone());

    let mut power = delegated_power.get(delegate.clone()).unwrap_or(0);
    power = power.checked_sub(prev_amount).unwrap_or(0);
    if power > 0 {
        delegated_power.set(delegate.clone(), power);
    } else {
        delegated_power.remove(delegate.clone());
    }

    let mut count = delegated_count.get(delegate.clone()).unwrap_or(0);
    count = count.saturating_sub(1);
    if count > 0 {
        delegated_count.set(delegate.clone(), count);
    } else {
        delegated_count.remove(delegate.clone());
    }

    env.storage().instance().set(&delegations_key, &delegations);
    env.storage().instance().set(&delegated_power_key, &delegated_power);
    env.storage().instance().set(&delegated_count_key, &delegated_count);
    env.storage().instance().set(&delegated_amounts_key, &delegated_amounts);

    env.events().publish(
        (Symbol::new(&env, "votes_undelegated"), owner),
        (delegate, prev_amount),
    );
}

/// Returns the representative `owner` currently delegates to, if any.
pub fn get_delegation(env: &Env, owner: &Address) -> Option<Address> {
    let delegations: soroban_sdk::Map<Address, Address> = env
        .storage()
        .instance()
        .get(&Symbol::new(env, "gov_delegations"))
        .unwrap_or(soroban_sdk::Map::new(env));
    delegations.get(owner.clone())
}

/// Number of holders currently delegating to `delegate`.
pub fn get_delegated_count(env: &Env, delegate: &Address) -> u32 {
    let delegated_count: soroban_sdk::Map<Address, u32> = env
        .storage()
        .instance()
        .get(&Symbol::new(env, "gov_delegated_count"))
        .unwrap_or(soroban_sdk::Map::new(env));
    delegated_count.get(delegate.clone()).unwrap_or(0)
}

/// Total voting power `delegate` currently holds on behalf of its delegators.
pub fn get_delegated_voting_power(env: &Env, delegate: &Address) -> i128 {
    let delegated_power: soroban_sdk::Map<Address, i128> = env
        .storage()
        .instance()
        .get(&Symbol::new(env, "gov_delegated_power"))
        .unwrap_or(soroban_sdk::Map::new(env));
    delegated_power.get(delegate.clone()).unwrap_or(0)
}

/// Effective voting power = own power (unless delegated away) + power received
/// from direct delegators. Chains are impossible by construction (depth <= 1).
pub fn get_effective_voting_power(env: &Env, address: &Address, own_voting_power: i128) -> i128 {
    let delegations: soroban_sdk::Map<Address, Address> = env
        .storage()
        .instance()
        .get(&Symbol::new(env, "gov_delegations"))
        .unwrap_or(soroban_sdk::Map::new(env));

    let received = get_delegated_voting_power(env, address);
    if delegations.contains_key(address.clone()) {
        // Own power has been delegated away; only incoming power counts here.
        received
    } else {
        own_voting_power + received
    }
}



pub fn get_proposal_vote_power(
    env: &Env,
    proposal_id: u64,
    address: &Address,
    balance: i128,
) -> VotePowerInfo {
    let proposal = read_proposal(env, proposal_id);
    let effective_votes = match proposal.voting_model {
        VotingModel::Linear => balance,
        VotingModel::Quadratic => integer_sqrt(balance),
        VotingModel::Delegated => get_effective_voting_power(env, address, balance),
    };

    VotePowerInfo {
        raw_votes: balance,
        effective_votes,
        voting_model: proposal.voting_model,
    }
}
