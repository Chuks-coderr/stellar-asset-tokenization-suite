#![cfg(test)]

use soroban_sdk::{
    testutils::Address as _,
    Address, Env, Map, Symbol,
};

use crate::{
    compliance_registry::{ComplianceRegistry, ComplianceRegistryClient},
    rwa_token::{RWAToken, RWATokenClient},
};

// ── helpers ──────────────────────────────────────────────────────────────────

struct TestEnv {
    env: Env,
    admin: Address,
    token: RWATokenClient<'static>,
    compliance: ComplianceRegistryClient<'static>,
}

fn setup() -> TestEnv {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);

    // Deploy compliance registry
    let compliance_id = env.register_contract(None, ComplianceRegistry);
    let compliance = ComplianceRegistryClient::new(&env, &compliance_id);
    compliance.initialize(&admin, &admin, &false, &false); // kyc_required = false for simplicity

    // Deploy RWA token
    let token_id = env.register_contract(None, RWAToken);
    let token = RWATokenClient::new(&env, &token_id);
    token.initialize(
        &admin,
        &Symbol::new(&env, "TestToken"),
        &Symbol::new(&env, "TT"),
        &1_000_000i128,
        &6u32,
        &Symbol::new(&env, "real_estate"),
        &Map::new(&env),
        &compliance_id,
        &admin, // placeholder dividend distributor
    );

    // SAFETY: env lifetime is tied to the test scope.
    let token: RWATokenClient<'static> = unsafe { core::mem::transmute(token) };
    let compliance: ComplianceRegistryClient<'static> = unsafe { core::mem::transmute(compliance) };

    TestEnv { env, admin, token, compliance }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #109: holder views
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn holder_count_counts_the_initial_holder() {
    let t = setup();
    assert_eq!(t.token.get_holder_count(), 1);
}

#[test]
fn holder_count_follows_mints_and_transfers() {
    let t = setup();
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);

    t.token.mint(&t.admin, &alice, &100i128);
    assert_eq!(t.token.get_holder_count(), 2);

    t.token.transfer(&t.admin, &alice, &bob, &40i128);
    // Alice still holds 60, so both of them remain holders.
    assert_eq!(t.token.get_holder_count(), 3);

    t.token.transfer(&t.admin, &alice, &bob, &60i128);
    // Alice is back to zero and is no longer a holder.
    assert_eq!(t.token.get_holder_count(), 2);
}

#[test]
fn holder_count_drops_when_a_holder_burns_everything() {
    let t = setup();
    let alice = Address::generate(&t.env);

    t.token.mint(&t.admin, &alice, &100i128);
    assert_eq!(t.token.get_holder_count(), 2);

    t.token.burn(&alice, &100i128);
    assert_eq!(t.token.get_holder_count(), 1);
}

#[test]
fn top_holders_are_ordered_by_balance_descending() {
    let t = setup();
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);

    t.token.mint(&t.admin, &alice, &100i128);
    t.token.mint(&t.admin, &bob, &300i128);

    let top = t.token.get_top_holders(&10u32);
    assert_eq!(top.len(), 3);
    // The admin holds 1_000_100, Bob 300, Alice 100.
    assert_eq!(top.get(0).unwrap().amount, 1_000_100);
    assert_eq!(top.get(1).unwrap().amount, 300);
    assert_eq!(top.get(2).unwrap().amount, 100);
    assert_eq!(top.get(1).unwrap().address, bob);
    assert_eq!(top.get(2).unwrap().address, alice);
}

#[test]
fn top_holders_respect_the_limit() {
    let t = setup();
    let alice = Address::generate(&t.env);
    let bob = Address::generate(&t.env);

    t.token.mint(&t.admin, &alice, &100i128);
    t.token.mint(&t.admin, &bob, &300i128);

    let top = t.token.get_top_holders(&2u32);
    assert_eq!(top.len(), 2);
    assert_eq!(top.get(0).unwrap().amount, 1_000_100);
    assert_eq!(top.get(1).unwrap().amount, 300);
}

#[test]
fn top_holders_cap_the_limit_at_100() {
    let t = setup();
    // The limit is capped rather than rejected.
    let top = t.token.get_top_holders(&1_000u32);
    assert_eq!(top.len(), 1);
}

#[test]
fn top_holders_with_a_zero_limit_is_empty() {
    let t = setup();
    let top = t.token.get_top_holders(&0u32);
    assert_eq!(top.len(), 0);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #107: forced transfer
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn force_transfer_moves_tokens_without_the_holder_consent() {
    let t = setup();
    let holder = Address::generate(&t.env);
    let recipient = Address::generate(&t.env);

    t.token.mint(&t.admin, &holder, &1_000i128);

    t.token.force_transfer(
        &t.admin,
        &holder,
        &recipient,
        &400i128,
        &Symbol::new(&t.env, "court_order"),
    );

    assert_eq!(t.token.get_balance(&holder).amount, 600);
    assert_eq!(t.token.get_balance(&recipient).amount, 400);
}

#[test]
fn force_transfer_seizes_from_a_blacklisted_holder() {
    // A seizure is the compliance regime acting: the holder does not
    // currently comply, so the compliance check must not block it.
    let t = setup();
    let holder = Address::generate(&t.env);
    let recipient = Address::generate(&t.env);

    t.token.mint(&t.admin, &holder, &1_000i128);
    t.compliance
        .add_to_blacklist(&t.admin, &holder, &Symbol::new(&t.env, "sanctioned"));

    t.token.force_transfer(
        &t.admin,
        &holder,
        &recipient,
        &1_000i128,
        &Symbol::new(&t.env, "sanction"),
    );

    assert_eq!(t.token.get_balance(&recipient).amount, 1_000);
}

#[test]
#[should_panic]
fn force_transfer_rejects_a_non_admin() {
    let t = setup();
    let holder = Address::generate(&t.env);
    let recipient = Address::generate(&t.env);
    let attacker = Address::generate(&t.env);

    t.token.mint(&t.admin, &holder, &1_000i128);

    t.token.force_transfer(
        &attacker,
        &holder,
        &recipient,
        &1_000i128,
        &Symbol::new(&t.env, "steal"),
    );
}

#[test]
#[should_panic]
fn force_transfer_rejects_a_paused_token() {
    let t = setup();
    let holder = Address::generate(&t.env);
    let recipient = Address::generate(&t.env);

    t.token.mint(&t.admin, &holder, &1_000i128);
    t.token.pause(&t.admin);

    t.token.force_transfer(
        &t.admin,
        &holder,
        &recipient,
        &1_000i128,
        &Symbol::new(&t.env, "seizure"),
    );
}

#[test]
#[should_panic]
fn force_transfer_rejects_more_than_the_balance() {
    let t = setup();
    let holder = Address::generate(&t.env);
    let recipient = Address::generate(&t.env);

    t.token.mint(&t.admin, &holder, &100i128);

    t.token.force_transfer(
        &t.admin,
        &holder,
        &recipient,
        &101i128,
        &Symbol::new(&t.env, "seizure"),
    );
}
