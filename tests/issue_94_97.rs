//! End-to-end coverage for issues #94, #95, #96 and #97.
//!
//! These live in `tests/` so they exercise the public contract API exactly the
//! way an integrator would.

use soroban_sdk::testutils::storage::{Instance as _, Persistent as _};
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::{Address, Env, Map, String, Symbol, Vec};

use stellar_asset_tokenization_suite::asset_factory::{
    AssetClass, AssetConfig, AssetFactory, AssetFactoryClient, ComplianceRules, DividendSchedule,
};
use stellar_asset_tokenization_suite::compliance_registry::{
    ComplianceRegistry, ComplianceRegistryClient,
};
use stellar_asset_tokenization_suite::rwa_token::{RWAToken, RWATokenClient, VestingType};

struct TokenFixture {
    env: Env,
    admin: Address,
    token: RWATokenClient<'static>,
}

fn token_fixture() -> TokenFixture {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let compliance_id = env.register_contract(None, ComplianceRegistry);
    let compliance = ComplianceRegistryClient::new(&env, &compliance_id);
    compliance.initialize(&admin, &admin, &false, &false);

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
        &admin,
    );

    // SAFETY: fixture keeps `env` alive for the lifetime of the test.
    let token: RWATokenClient<'static> = unsafe { core::mem::transmute(token) };

    TokenFixture { env, admin, token }
}

fn metadata_updates(env: &Env, entries: &[(&str, &str)]) -> Map<Symbol, String> {
    let mut updates = Map::new(env);
    for (key, value) in entries {
        updates.set(Symbol::new(env, key), String::from_str(env, value));
    }
    updates
}

// ── #97: metadata ────────────────────────────────────────────────────────────

#[test]
fn update_metadata_changes_only_mutable_fields() {
    let f = token_fixture();
    let updates = metadata_updates(
        &f.env,
        &[
            ("description", "Updated description"),
            ("issuer", "Acme Inc"),
        ],
    );

    f.token.update_metadata(&f.admin, &updates);

    let info = f.token.get_token_info();
    assert_eq!(
        info.metadata.get(Symbol::new(&f.env, "description")),
        Some(String::from_str(&f.env, "Updated description"))
    );
    assert_eq!(
        info.metadata.get(Symbol::new(&f.env, "issuer")),
        Some(String::from_str(&f.env, "Acme Inc"))
    );
    // Immutable token fields are untouched.
    assert_eq!(info.name, Symbol::new(&f.env, "TestToken"));
    assert_eq!(info.symbol, Symbol::new(&f.env, "TT"));
    assert_eq!(info.decimals, 6);
}

#[test]
#[should_panic]
fn update_metadata_rejects_immutable_fields() {
    let f = token_fixture();
    let updates = metadata_updates(&f.env, &[("name", "Hacked")]);
    f.token.update_metadata(&f.admin, &updates);
}

#[test]
#[should_panic]
fn update_metadata_rejects_too_many_keys() {
    let f = token_fixture();
    let mut updates = Map::new(&f.env);
    for i in 0..51u32 {
        updates.set(
            Symbol::new(&f.env, &format!("key{}", i)),
            String::from_str(&f.env, "value"),
        );
    }
    f.token.update_metadata(&f.admin, &updates);
}

#[test]
#[should_panic]
fn update_metadata_rejects_too_long_values() {
    let f = token_fixture();
    let long_value = "a".repeat(257);
    let mut updates = Map::new(&f.env);
    updates.set(
        Symbol::new(&f.env, "description"),
        String::from_str(&f.env, &long_value),
    );
    f.token.update_metadata(&f.admin, &updates);
}

// ── #95: allowances ─────────────────────────────────────────────────────────

#[test]
fn approve_and_transfer_from_respects_and_decrements_allowance() {
    let f = token_fixture();
    let spender = Address::generate(&f.env);
    let recipient = Address::generate(&f.env);

    f.token.approve(&f.admin, &spender, &1_000i128);
    assert_eq!(f.token.allowance(&f.admin, &spender), 1_000);

    f.token
        .transfer_from(&spender, &f.admin, &recipient, &400i128);

    assert_eq!(f.token.get_balance(&recipient).amount, 400);
    assert_eq!(f.token.allowance(&f.admin, &spender), 600);
}

#[test]
fn zero_allowance_revokes_and_reports_zero() {
    let f = token_fixture();
    let spender = Address::generate(&f.env);

    f.token.approve(&f.admin, &spender, &500i128);
    f.token.approve(&f.admin, &spender, &0i128);

    assert_eq!(f.token.allowance(&f.admin, &spender), 0);
}

#[test]
#[should_panic]
fn transfer_from_without_allowance_panics() {
    let f = token_fixture();
    let spender = Address::generate(&f.env);
    let recipient = Address::generate(&f.env);

    f.token
        .transfer_from(&spender, &f.admin, &recipient, &1i128);
}

#[test]
#[should_panic]
fn allowance_cannot_exceed_spendable_balance() {
    let f = token_fixture();
    let spender = Address::generate(&f.env);

    f.token.approve(&f.admin, &spender, &1_000_001i128);
}

#[test]
fn mint_and_burn_do_not_change_allowance() {
    let f = token_fixture();
    let spender = Address::generate(&f.env);

    f.token.approve(&f.admin, &spender, &1_000i128);
    f.token.mint(&f.admin, &f.admin, &500i128);
    f.token.burn(&f.admin, &100i128);

    assert_eq!(f.token.allowance(&f.admin, &spender), 1_000);
}

// ── #96: vesting locks ──────────────────────────────────────────────────────

#[test]
fn cliff_lock_unlocks_only_at_expiry() {
    let f = token_fixture();
    f.env.ledger().set_timestamp(1_000);

    f.token.lock_tokens(&f.admin, &f.admin, &1_000i128, &100u64);

    let during = f.token.get_balance(&f.admin);
    assert_eq!(during.amount, 1_000_000);
    assert_eq!(during.locked_amount, 1_000);
    assert_eq!(f.token.get_spendable_balance(&f.admin), 999_000);

    f.env.ledger().set_timestamp(1_099);
    assert_eq!(f.token.get_balance(&f.admin).locked_amount, 1_000);

    f.env.ledger().set_timestamp(1_100);
    let after = f.token.get_balance(&f.admin);
    assert_eq!(after.locked_amount, 0);
    assert_eq!(f.token.get_spendable_balance(&f.admin), 1_000_000);
    assert_eq!(f.token.get_lock_slots(&f.admin).len(), 0);
}

#[test]
fn linear_vesting_unlocks_evenly_and_auto_prunes() {
    let f = token_fixture();
    f.env.ledger().set_timestamp(2_000);

    f.token.lock_tokens_with_vesting(
        &f.admin,
        &f.admin,
        &1_000i128,
        &100u64,
        &VestingType::Linear,
    );

    // 50% through the schedule, 50% remains locked.
    f.env.ledger().set_timestamp(2_050);
    let mid = f.token.get_balance(&f.admin);
    assert_eq!(mid.amount, 1_000_000);
    assert_eq!(mid.locked_amount, 500);
    assert_eq!(f.token.get_spendable_balance(&f.admin), 999_500);

    // Fully vested: slot is pruned on the balance read.
    f.env.ledger().set_timestamp(2_100);
    let end = f.token.get_balance(&f.admin);
    assert_eq!(end.locked_amount, 0);
    assert_eq!(end.voting_power, 0);
    assert_eq!(f.token.get_lock_slots(&f.admin).len(), 0);
}

#[test]
fn multiple_locks_are_tracked_independently() {
    let f = token_fixture();
    f.env.ledger().set_timestamp(3_000);

    f.token.lock_tokens(&f.admin, &f.admin, &1_000i128, &100u64);
    f.token.lock_tokens_with_vesting(
        &f.admin,
        &f.admin,
        &2_000i128,
        &200u64,
        &VestingType::Linear,
    );

    f.env.ledger().set_timestamp(3_100);
    // Cliff slot expired; linear slot is 50% vested (1_000 locked).
    let balance = f.token.get_balance(&f.admin);
    assert_eq!(balance.locked_amount, 1_000);
    assert_eq!(f.token.get_lock_slots(&f.admin).len(), 1);
}

// ── #94: factory storage ────────────────────────────────────────────────────

fn factory_fixture() -> (Env, Address, Address, AssetFactoryClient<'static>) {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let factory_id = env.register_contract(None, AssetFactory);
    let client = AssetFactoryClient::new(&env, &factory_id);
    client.initialize(&admin, &admin);
    let client: AssetFactoryClient<'static> = unsafe { core::mem::transmute(client) };
    (env, admin, factory_id, client)
}

#[test]
fn registry_uses_persistent_storage_with_extended_ttl() {
    let (env, _admin, factory_id, client) = factory_fixture();

    env.as_contract(&factory_id, || {
        let registry_key = Symbol::new(&env, "registry");
        let persistent_ttl = env.storage().persistent().get_ttl(&registry_key);
        let instance_ttl = env.storage().instance().get_ttl();

        assert!(persistent_ttl > 0, "persistent registry TTL was not set");
        assert!(instance_ttl > 0, "instance TTL was not set");
    });

    // A read also refreshes the persistent registry entry.
    let assets = client.get_all_assets();
    assert_eq!(assets.len(), 0);
    assert_eq!(client.get_asset_count(), 0);

    env.as_contract(&factory_id, || {
        let registry_key = Symbol::new(&env, "registry");
        assert!(env.storage().persistent().get_ttl(&registry_key) > 0);
    });
}

#[test]
fn templates_and_counters_remain_in_instance_storage() {
    let (env, admin, _factory_id, client) = factory_fixture();

    let template = stellar_asset_tokenization_suite::asset_factory::AssetTemplate {
        asset_class: AssetClass::RealEstate,
        base_config: AssetConfig {
            name: Symbol::new(&env, "TestAsset"),
            symbol: Symbol::new(&env, "TA"),
            decimals: 6,
            total_supply: 1_000_000,
            asset_class: AssetClass::RealEstate,
            compliance_rules: ComplianceRules {
                kyc_required: false,
                accredited_investor_only: false,
                geographic_restrictions: Vec::new(&env),
                holding_period_days: 0,
                transfer_limits: 1_000_000,
            },
            dividend_schedule: Vec::<DividendSchedule>::new(&env),
            metadata: Map::new(&env),
        },
        wasm_hash: soroban_sdk::BytesN::from_array(&env, &[0u8; 32]),
        is_active: true,
        version: 1,
    };

    client.register_template(&admin, &template);
    assert_eq!(client.get_template(&AssetClass::RealEstate).version, 1);
    // Registering a template does not create an asset or bump the counter.
    assert_eq!(client.get_asset_count(), 0);
}
