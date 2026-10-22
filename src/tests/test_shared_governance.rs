#![cfg(test)]

use soroban_sdk::{
    testutils::{Address as _, Events as _},
    Address, BytesN, Env, Map, Symbol, TryFromVal as _, TryIntoVal as _, Vec,
};

use crate::{
    compliance_registry::{ComplianceRegistry, ComplianceRegistryClient},
    rwa_token::{RWAToken, RWATokenClient},
    shared_governance,
};

// ── helpers ──────────────────────────────────────────────────────────────────

struct GovEnv {
    env: Env,
    admin: Address,
    owner2: Address,
    token: RWATokenClient<'static>,
}

fn setup() -> GovEnv {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let owner2 = Address::generate(&env);

    let compliance_id = env.register_contract(None, ComplianceRegistry);
    let compliance = ComplianceRegistryClient::new(&env, &compliance_id);
    compliance.initialize(&admin, &admin, &false, &false);

    let token_id = env.register_contract(None, RWAToken);
    let token = RWATokenClient::new(&env, &token_id);
    token.initialize(
        &admin,
        &Symbol::new(&env, "GovToken"),
        &Symbol::new(&env, "GT"),
        &1_000_000i128,
        &6u32,
        &Symbol::new(&env, "real_estate"),
        &Map::new(&env),
        &compliance_id,
        &admin,
    );

    // SAFETY: env lifetime is tied to the test scope.
    let token: RWATokenClient<'static> = unsafe { core::mem::transmute(token) };

    GovEnv { env, admin, owner2, token }
}

fn init_governance(g: &GovEnv, owners: &Vec<Address>, threshold: u32, timelock: u64) {
    shared_governance::write_governance(&g.env, &g.admin, owners, threshold, timelock);
}

fn assert_event_published(env: &Env, name: &str) {
    let mut published = false;
    for event in env.events().all().iter() {
        let topics = event.1;
        let topic = topics.get(0);
        if topic.is_some() {
            let sym: Option<Symbol> = topic.unwrap().try_into_val(env).ok();
            if sym.is_some() && sym.unwrap() == Symbol::new(env, name) {
                published = true;
                break;
            }
        }
    }
    assert!(published, "expected event `{name}` to be published");
}

// ── initialize + create_proposal ─────────────────────────────────────────────

#[test]
fn initialize_sets_storage_and_creates_proposal() {
    let g = setup();
    let owners = Vec::from_array(&g.env, [g.admin.clone(), g.owner2.clone()]);
    init_governance(&g, &owners, 1, 0);

    let id = shared_governance::create_proposal(
        g.env.clone(),
        g.owner2.clone(),
        Symbol::new(&g.env, "upgrade"),
        BytesN::from_array(&g.env, &[7u8; 32]),
    );
    assert_eq!(id, 1);
}

#[test]
#[should_panic]
fn non_owner_cannot_create_proposal() {
    let g = setup();
    let owners = Vec::from_array(&g.env, [g.admin.clone()]);
    init_governance(&g, &owners, 1, 0);

    let stranger = Address::generate(&g.env);
    shared_governance::create_proposal(
        g.env.clone(),
        stranger,
        Symbol::new(&g.env, "upgrade"),
        BytesN::from_array(&g.env, &[7u8; 32]),
    );
}

// ── #189 min proposal balance ───────────────────────────────────────────────

#[test]
fn create_proposal_allowed_when_unlocked_balance_meets_minimum() {
    let g = setup();
    let owners = Vec::from_array(&g.env, [g.admin.clone(), g.owner2.clone()]);
    init_governance(&g, &owners, 1, 0);

    g.token.mint(&g.admin, &g.owner2, &100_000i128);
    shared_governance::set_min_proposal_balance(&g.env, g.admin.clone(), 50_000u128);
    shared_governance::set_stake_token(&g.env, g.admin.clone(), g.token.address.clone());

    let id = shared_governance::create_proposal(
        g.env.clone(),
        g.owner2.clone(),
        Symbol::new(&g.env, "upgrade"),
        BytesN::from_array(&g.env, &[7u8; 32]),
    );
    assert_eq!(id, 1);
}

#[test]
#[should_panic]
fn create_proposal_rejected_when_below_minimum() {
    let g = setup();
    let owners = Vec::from_array(&g.env, [g.admin.clone(), g.owner2.clone()]);
    init_governance(&g, &owners, 1, 0);

    g.token.mint(&g.admin, &g.owner2, &100_000i128);
    shared_governance::set_min_proposal_balance(&g.env, g.admin.clone(), 200_000u128);
    shared_governance::set_stake_token(&g.env, g.admin.clone(), g.token.address.clone());

    shared_governance::create_proposal(
        g.env.clone(),
        g.owner2.clone(),
        Symbol::new(&g.env, "upgrade"),
        BytesN::from_array(&g.env, &[7u8; 32]),
    );
}

#[test]
#[should_panic]
fn locked_tokens_do_not_count_toward_minimum() {
    let g = setup();
    let owners = Vec::from_array(&g.env, [g.admin.clone(), g.owner2.clone()]);
    init_governance(&g, &owners, 1, 0);

    // owner2 holds 100_000 total but 80_000 is locked -> 20_000 unlocked.
    g.token.mint(&g.admin, &g.owner2, &100_000i128);
    g.token.lock_tokens(&g.owner2, &g.owner2, &80_000i128, &3600u64);
    shared_governance::set_min_proposal_balance(&g.env, g.admin.clone(), 50_000u128);
    shared_governance::set_stake_token(&g.env, g.admin.clone(), g.token.address.clone());

    shared_governance::create_proposal(
        g.env.clone(),
        g.owner2.clone(),
        Symbol::new(&g.env, "upgrade"),
        BytesN::from_array(&g.env, &[7u8; 32]),
    );
}

#[test]
#[should_panic]
fn updating_minimum_changes_enforcement() {
    let g = setup();
    let owners = Vec::from_array(&g.env, [g.admin.clone(), g.owner2.clone()]);
    init_governance(&g, &owners, 1, 0);

    g.token.mint(&g.admin, &g.owner2, &100_000i128);
    shared_governance::set_stake_token(&g.env, g.admin.clone(), g.token.address.clone());

    // Below the new higher threshold -> rejected.
    shared_governance::set_min_proposal_balance(&g.env, g.admin.clone(), 150_000u128);
    shared_governance::create_proposal(
        g.env.clone(),
        g.owner2.clone(),
        Symbol::new(&g.env, "upgrade"),
        BytesN::from_array(&g.env, &[7u8; 32]),
    );
}

#[test]
fn lowering_minimum_allows_proposal_after_raise() {
    let g = setup();
    let owners = Vec::from_array(&g.env, [g.admin.clone(), g.owner2.clone()]);
    init_governance(&g, &owners, 1, 0);

    g.token.mint(&g.admin, &g.owner2, &100_000i128);
    shared_governance::set_stake_token(&g.env, g.admin.clone(), g.token.address.clone());

    // Too high initially, then lowered to something owner2 can satisfy.
    shared_governance::set_min_proposal_balance(&g.env, g.admin.clone(), 150_000u128);
    shared_governance::set_min_proposal_balance(&g.env, g.admin.clone(), 100_000u128);

    let id = shared_governance::create_proposal(
        g.env.clone(),
        g.owner2.clone(),
        Symbol::new(&g.env, "upgrade"),
        BytesN::from_array(&g.env, &[7u8; 32]),
    );
    assert_eq!(id, 1);
}

// ── #188 security council veto ───────────────────────────────────────────────

fn make_proposal(g: &GovEnv) -> u64 {
    let owners = Vec::from_array(&g.env, [g.admin.clone(), g.owner2.clone()]);
    init_governance(g, &owners, 1, 0);
    shared_governance::create_proposal(
        g.env.clone(),
        g.admin.clone(),
        Symbol::new(&g.env, "upgrade"),
        BytesN::from_array(&g.env, &[7u8; 32]),
    )
}

#[test]
fn council_update_publishes_event() {
    let g = setup();
    let _ = make_proposal(&g);

    let c1 = Address::generate(&g.env);
    let c2 = Address::generate(&g.env);
    shared_governance::update_council(
        &g.env,
        g.admin.clone(),
        Vec::from_array(&g.env, [c1, c2]),
        2,
    );

    assert_event_published(&g.env, "council_updated");
}

#[test]
fn council_veto_requires_threshold_of_votes() {
    let g = setup();
    let id = make_proposal(&g);

    let c1 = Address::generate(&g.env);
    let c2 = Address::generate(&g.env);
    shared_governance::update_council(
        &g.env,
        g.admin.clone(),
        Vec::from_array(&g.env, [c1.clone(), c2.clone()]),
        2,
    );

    shared_governance::approve(g.env.clone(), g.admin.clone(), id);
    assert!(shared_governance::can_execute(
        &g.env,
        id,
        Symbol::new(&g.env, "upgrade"),
        BytesN::from_array(&g.env, &[7u8; 32]),
    ));

    // First council vote: M-of-N (2-of-2) not yet reached.
    let votes = shared_governance::veto_proposal(g.env.clone(), c1, id);
    assert_eq!(votes, 1);
    assert!(!shared_governance::proposal_is_vetoed(&g.env, id));

    // Second council vote reaches the threshold and vetoes the proposal.
    let votes = shared_governance::veto_proposal(g.env.clone(), c2, id);
    assert_eq!(votes, 2);
    assert!(shared_governance::proposal_is_vetoed(&g.env, id));
    assert_event_published(&g.env, "proposal_vetoed");
}

#[test]
fn veto_threshold_not_reached_proposal_stays_executable() {
    let g = setup();
    let id = make_proposal(&g);

    let c1 = Address::generate(&g.env);
    let c2 = Address::generate(&g.env);
    shared_governance::update_council(
        &g.env,
        g.admin.clone(),
        Vec::from_array(&g.env, [c1.clone(), c2]),
        2,
    );

    shared_governance::approve(g.env.clone(), g.admin.clone(), id);

    // Below the M-of-N threshold no veto is applied yet.
    shared_governance::veto_proposal(g.env.clone(), c1, id);
    assert!(shared_governance::can_execute(
        &g.env,
        id,
        Symbol::new(&g.env, "upgrade"),
        BytesN::from_array(&g.env, &[7u8; 32]),
    ));
}

#[test]
#[should_panic]
fn non_council_member_cannot_veto() {
    let g = setup();
    let id = make_proposal(&g);

    let c1 = Address::generate(&g.env);
    shared_governance::update_council(
        &g.env,
        g.admin.clone(),
        Vec::from_array(&g.env, [c1]),
        1,
    );

    let outsider = Address::generate(&g.env);
    shared_governance::veto_proposal(g.env.clone(), outsider, id);
}

#[test]
fn single_member_council_vetoes_immediately() {
    let g = setup();
    let id = make_proposal(&g);

    let c1 = Address::generate(&g.env);
    shared_governance::update_council(
        &g.env,
        g.admin.clone(),
        Vec::from_array(&g.env, [c1.clone()]),
        1,
    );

    let votes = shared_governance::veto_proposal(g.env.clone(), c1, id);
    assert_eq!(votes, 1);
    assert!(shared_governance::proposal_is_vetoed(&g.env, id));
    assert_event_published(&g.env, "proposal_vetoed");
}

#[test]
#[should_panic]
fn duplicate_veto_vote_panics() {
    let g = setup();
    let id = make_proposal(&g);

    let c1 = Address::generate(&g.env);
    shared_governance::update_council(
        &g.env,
        g.admin.clone(),
        Vec::from_array(&g.env, [c1.clone()]),
        1,
    );

    shared_governance::veto_proposal(g.env.clone(), c1.clone(), id);
    shared_governance::veto_proposal(g.env.clone(), c1, id);
}

#[test]
fn vetoed_proposal_can_never_execute() {
    let g = setup();
    let id = make_proposal(&g);

    let c1 = Address::generate(&g.env);
    shared_governance::update_council(
        &g.env,
        g.admin.clone(),
        Vec::from_array(&g.env, [c1.clone()]),
        1,
    );

    shared_governance::veto_proposal(g.env.clone(), c1, id);

    assert!(!shared_governance::can_execute(
        &g.env,
        id,
        Symbol::new(&g.env, "upgrade"),
        BytesN::from_array(&g.env, &[7u8; 32]),
    ));
}

#[test]
#[should_panic]
fn executing_vetoed_proposal_panics() {
    let g = setup();
    let id = make_proposal(&g);

    let c1 = Address::generate(&g.env);
    shared_governance::update_council(
        &g.env,
        g.admin.clone(),
        Vec::from_array(&g.env, [c1.clone()]),
        1,
    );

    shared_governance::veto_proposal(g.env.clone(), c1, id);
    shared_governance::execute_mark(g.env.clone(), g.admin.clone(), id);
}