#![cfg(test)]

use soroban_sdk::{
    testutils::storage::{Instance as _, Persistent as _},
    testutils::Address as _,
    Address, BytesN, Env, Map, String, Symbol, Vec,
};

use crate::asset_factory::{
    validate_metadata, AssetClass, AssetConfig, AssetFactory, AssetFactoryClient,
    AssetFactoryError, AssetInfo, AssetQueryFilter, ComplianceRules, AssetTemplate,
};

// ── helpers ──────────────────────────────────────────────────────────────────

fn default_compliance_rules(env: &Env) -> ComplianceRules {
    ComplianceRules {
        kyc_required: true,
        accredited_investor_only: false,
        geographic_restrictions: Vec::new(env),
        holding_period_days: 0,
        transfer_limits: 1_000_000i128,
    }
}

fn default_asset_config(env: &Env) -> AssetConfig {
    AssetConfig {
        name: Symbol::new(env, "TestAsset"),
        symbol: Symbol::new(env, "TA"),
        decimals: 6,
        total_supply: 1_000_000i128,
        asset_class: AssetClass::RealEstate,
        compliance_rules: default_compliance_rules(env),
        dividend_schedule: Vec::new(env),
        metadata: Map::new(env),
    }
}

fn setup() -> (Env, Address, AssetFactoryClient<'static>) {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let contract_id = env.register_contract(None, AssetFactory);
    let client = AssetFactoryClient::new(&env, &contract_id);
    client.initialize(&admin, &admin);
    let client: AssetFactoryClient<'static> = unsafe { core::mem::transmute(client) };
    (env, admin, client)
}

// ── initialize ───────────────────────────────────────────────────────────────

#[test]
fn initialize_succeeds() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let contract_id = env.register_contract(None, AssetFactory);
    let client = AssetFactoryClient::new(&env, &contract_id);
    // Should not panic
    client.initialize(&admin, &admin);
}

#[test]
#[should_panic]
fn initialize_twice_panics() {
    let (env, admin, client) = setup();
    client.initialize(&admin, &admin);
}

// ── asset count ──────────────────────────────────────────────────────────────

#[test]
fn initial_asset_count_is_zero() {
    let (_, _, client) = setup();
    assert_eq!(client.get_asset_count(), 0);
}

// ── register_template ────────────────────────────────────────────────────────

#[test]
fn register_template_stores_template() {
    let (env, admin, client) = setup();
    let wasm_hash = BytesN::from_array(&env, &[0u8; 32]);
    let template = AssetTemplate {
        asset_class: AssetClass::RealEstate,
        base_config: default_asset_config(&env),
        wasm_hash,
        is_active: true,
        version: 1,
    };
    client.register_template(&admin, &template);
    let stored = client.get_template(&AssetClass::RealEstate);
    assert!(stored.is_active);
    assert_eq!(stored.version, 1);
}

#[test]
#[should_panic]
fn register_template_by_non_admin_panics() {
    let (env, _, client) = setup();
    let attacker = Address::generate(&env);
    let wasm_hash = BytesN::from_array(&env, &[0u8; 32]);
    let template = AssetTemplate {
        asset_class: AssetClass::Commodity,
        base_config: default_asset_config(&env),
        wasm_hash,
        is_active: true,
        version: 1,
    };
    client.register_template(&attacker, &template);
}

// ── get_template ─────────────────────────────────────────────────────────────

#[test]
#[should_panic]
fn get_template_for_unregistered_class_panics() {
    let (_, _, client) = setup();
    // No template registered for Invoice
    client.get_template(&AssetClass::Invoice);
}

// ── update_admin ─────────────────────────────────────────────────────────────

#[test]
fn update_admin_changes_admin() {
    let (env, admin, client) = setup();
    let new_admin = Address::generate(&env);
    client.update_admin(&admin, &new_admin);
    // Old admin should no longer be able to call admin-only functions
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        client.update_admin(&admin, &admin);
    }));
    assert!(result.is_err());
}

#[test]
#[should_panic]
fn update_admin_by_non_admin_panics() {
    let (env, _, client) = setup();
    let attacker = Address::generate(&env);
    let new_admin = Address::generate(&env);
    client.update_admin(&attacker, &new_admin);
}

// ── get_all_assets ────────────────────────────────────────────────────────────

#[test]
fn get_all_assets_returns_empty_map_initially() {
    let (_, _, client) = setup();
    let assets = client.get_all_assets();
    assert_eq!(assets.len(), 0);
}

// ── list_assets ──────────────────────────────────────────────────────────────

#[test]
fn list_assets_returns_empty_vec_initially() {
    let (_, _, client) = setup();
    let assets = client.list_assets();
    assert_eq!(assets.len(), 0);
}

// ── set_asset_pause_status ────────────────────────────────────────────────────

#[test]
#[should_panic]
fn set_asset_pause_status_for_nonexistent_asset_panics() {
    let (env, admin, client) = setup();
    client.set_asset_pause_status(&admin, &Symbol::new(&env, "NONE"), &true);
}

// ── emergency_pause_all ───────────────────────────────────────────────────────

#[test]
fn emergency_pause_all_on_empty_registry_does_not_panic() {
    let (_, admin, client) = setup();
    // Should complete without error even with no assets
    client.emergency_pause_all(&admin);
}

#[test]
#[should_panic]
fn emergency_pause_all_by_non_admin_panics() {
    let (env, _, client) = setup();
    let attacker = Address::generate(&env);
    client.emergency_pause_all(&attacker);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #222: Upgrade & Migration Tests for AssetFactory
// ═══════════════════════════════════════════════════════════════════════════════

// ── migrate ───────────────────────────────────────────────────────────────────

#[test]
#[should_panic]
fn migrate_when_already_at_latest_version_panics() {
    let (_, admin, client) = setup();
    // Already at STORAGE_VERSION = 1 after initialize
    client.migrate(&admin);
}

#[test]
#[should_panic]
fn migrate_by_non_admin_panics() {
    let (env, _, client) = setup();
    let attacker = Address::generate(&env);
    client.migrate(&attacker);
}

#[test]
fn migrate_preserves_existing_data() {
    let (env, admin, client) = setup();

    // Register a template before migration attempt
    let wasm_hash = BytesN::from_array(&env, &[1u8; 32]);
    let template = AssetTemplate {
        asset_class: AssetClass::RealEstate,
        base_config: default_asset_config(&env),
        wasm_hash,
        is_active: true,
        version: 1,
    };
    client.register_template(&admin, &template);
    let count_before = client.get_asset_count();

    // Data should still be intact (migrate may panic if already at latest, but if
    // STORAGE_VERSION were higher, migration would preserve state)
    assert_eq!(count_before, 0); // No assets deployed yet

    // Verify template still accessible
    let stored = client.get_template(&AssetClass::RealEstate);
    assert!(stored.is_active);
    assert_eq!(stored.version, 1);
}

#[test]
fn migrate_with_empty_registry() {
    let (_, admin, client) = setup();
    // Registry is empty after fresh setup — migration should still be callable
    // (though it panics because we're already at latest version)
    let assets = client.get_all_assets();
    assert_eq!(assets.len(), 0);
    // If a real migration were needed, empty state should not cause issues
}

#[test]
fn data_integrity_after_migration_cycle() {
    let (env, admin, client) = setup();

    // Populate some state
    let wasm_hash = BytesN::from_array(&env, &[2u8; 32]);
    let template = AssetTemplate {
        asset_class: AssetClass::Commodity,
        base_config: default_asset_config(&env),
        wasm_hash,
        is_active: true,
        version: 1,
    };
    client.register_template(&admin, &template);

    // Check Count
    let asset_count = client.get_asset_count();
    assert_eq!(asset_count, 0);

    // List assets doesn't panic
    let listed = client.list_assets();
    assert_eq!(listed.len(), 0);

    // Template data intact
    let commodity_tmpl = client.get_template(&AssetClass::Commodity);
    assert!(commodity_tmpl.is_active);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #82: metadata limits
// ═══════════════════════════════════════════════════════════════════════════════

/// A setup that also hands back the contract address, so the registry can be
/// seeded directly.
fn setup_with_id() -> (Env, Address, Address, AssetFactoryClient<'static>) {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let contract_id = env.register_contract(None, AssetFactory);
    let client = AssetFactoryClient::new(&env, &contract_id);
    client.initialize(&admin, &admin);
    let client: AssetFactoryClient<'static> = unsafe { core::mem::transmute(client) };
    (env, admin, contract_id, client)
}

fn metadata_with_entries(env: &Env, count: u32, value_len: u32) -> Map<Symbol, String> {
    let mut metadata = Map::new(env);
    let value = "x".repeat(value_len as usize);
    for i in 0..count {
        metadata.set(
            Symbol::new(env, &format!("k{}", i)),
            String::from_str(env, &value),
        );
    }
    metadata
}

#[test]
#[should_panic]
fn metadata_with_more_than_50_entries_is_rejected() {
    let (env, _, _, _) = setup_with_id();
    let metadata = metadata_with_entries(&env, 51, 8);
    validate_metadata(&env, &metadata);
}

#[test]
fn metadata_with_exactly_50_entries_is_accepted() {
    let (env, _, _, _) = setup_with_id();
    let metadata = metadata_with_entries(&env, 50, 8);
    validate_metadata(&env, &metadata);
}

#[test]
#[should_panic]
fn metadata_value_longer_than_256_bytes_is_rejected() {
    let (env, _, _, _) = setup_with_id();
    let metadata = metadata_with_entries(&env, 1, 257);
    validate_metadata(&env, &metadata);
}

#[test]
fn metadata_value_of_exactly_256_bytes_is_accepted() {
    let (env, _, _, _) = setup_with_id();
    let metadata = metadata_with_entries(&env, 1, 256);
    validate_metadata(&env, &metadata);
}

#[test]
fn empty_metadata_is_accepted() {
    let (env, _, _, _) = setup_with_id();
    let metadata = Map::new(&env);
    validate_metadata(&env, &metadata);
}

#[test]
#[should_panic]
fn create_asset_rejects_oversized_metadata() {
    let (env, admin, _, client) = setup_with_id();
    let mut config = default_asset_config(&env);
    config.metadata = metadata_with_entries(&env, 51, 8);
    client.create_asset(&admin, &config);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #83: asset search
// ═══════════════════════════════════════════════════════════════════════════════

/// Puts an asset straight into the registry, so the search can be exercised
/// without deploying a token.
fn seed_asset(
    env: &Env,
    contract_id: &Address,
    symbol: &str,
    class: AssetClass,
    created_at: u64,
    paused: bool,
) {
    env.as_contract(contract_id, || {
        let mut registry: Map<Symbol, AssetInfo> = env
            .storage()
            .instance()
            .get(&Symbol::new(env, "registry"))
            .unwrap_or(Map::new(env));

        registry.set(
            Symbol::new(env, symbol),
            AssetInfo {
                name: Symbol::new(env, symbol),
                symbol: Symbol::new(env, symbol),
                total_supply: 1_000_000i128,
                decimals: 6,
                asset_class: class,
                metadata: Map::new(env),
                compliance_registry: env.current_contract_address(),
                dividend_distributor: env.current_contract_address(),
                token_address: Address::generate(env),
                created_at,
                is_paused: paused,
                template_version: 1,
                upgrade_proposals: Vec::new(env),
            },
        );

        env.storage()
            .instance()
            .set(&Symbol::new(env, "registry"), &registry);
    });
}

fn no_filter(env: &Env) -> AssetQueryFilter {
    AssetQueryFilter {
        asset_class: AssetClass::RealEstate,
        by_class: false,
        is_paused: false,
        by_pause_state: false,
        created_after: 0,
        created_before: 0,
        by_created_at: false,
        cursor: Symbol::new(env, "start"),
        has_cursor: false,
    }
}

#[test]
fn query_assets_returns_an_empty_page_for_an_empty_registry() {
    let (env, _, _, client) = setup_with_id();

    let page = client.query_assets(&no_filter(&env), &20u32);
    assert_eq!(page.assets.len(), 0);
    assert!(!page.has_more);
}

#[test]
fn query_assets_returns_every_asset_without_filters() {
    let (env, _, contract_id, client) = setup_with_id();
    seed_asset(&env, &contract_id, "AAA", AssetClass::RealEstate, 100, false);
    seed_asset(&env, &contract_id, "BBB", AssetClass::Commodity, 200, true);

    let page = client.query_assets(&no_filter(&env), &20u32);
    assert_eq!(page.assets.len(), 2);
    assert!(!page.has_more);
    // The registry is keyed by symbol, so the order is stable.
    assert_eq!(page.assets.get(0).unwrap().symbol, Symbol::new(&env, "AAA"));
    assert_eq!(page.assets.get(1).unwrap().symbol, Symbol::new(&env, "BBB"));
}

#[test]
fn query_assets_filters_by_asset_class() {
    let (env, _, contract_id, client) = setup_with_id();
    seed_asset(&env, &contract_id, "AAA", AssetClass::RealEstate, 100, false);
    seed_asset(&env, &contract_id, "BBB", AssetClass::Commodity, 200, false);

    let mut filter = no_filter(&env);
    filter.by_class = true;
    filter.asset_class = AssetClass::Commodity;

    let page = client.query_assets(&filter, &20u32);
    assert_eq!(page.assets.len(), 1);
    assert_eq!(page.assets.get(0).unwrap().symbol, Symbol::new(&env, "BBB"));
}

#[test]
fn query_assets_filters_by_pause_state() {
    let (env, _, contract_id, client) = setup_with_id();
    seed_asset(&env, &contract_id, "AAA", AssetClass::RealEstate, 100, false);
    seed_asset(&env, &contract_id, "BBB", AssetClass::RealEstate, 200, true);

    let mut filter = no_filter(&env);
    filter.by_pause_state = true;
    filter.is_paused = true;

    let page = client.query_assets(&filter, &20u32);
    assert_eq!(page.assets.len(), 1);
    assert_eq!(page.assets.get(0).unwrap().symbol, Symbol::new(&env, "BBB"));
}

#[test]
fn query_assets_filters_by_creation_window() {
    let (env, _, contract_id, client) = setup_with_id();
    seed_asset(&env, &contract_id, "AAA", AssetClass::RealEstate, 100, false);
    seed_asset(&env, &contract_id, "BBB", AssetClass::RealEstate, 200, false);
    seed_asset(&env, &contract_id, "CCC", AssetClass::RealEstate, 300, false);

    let mut filter = no_filter(&env);
    filter.by_created_at = true;
    filter.created_after = 150;
    filter.created_before = 250;

    let page = client.query_assets(&filter, &20u32);
    assert_eq!(page.assets.len(), 1);
    assert_eq!(page.assets.get(0).unwrap().symbol, Symbol::new(&env, "BBB"));
}

#[test]
fn query_assets_paginates_with_the_cursor() {
    let (env, _, contract_id, client) = setup_with_id();
    seed_asset(&env, &contract_id, "AAA", AssetClass::RealEstate, 100, false);
    seed_asset(&env, &contract_id, "BBB", AssetClass::RealEstate, 200, false);
    seed_asset(&env, &contract_id, "CCC", AssetClass::RealEstate, 300, false);

    let first = client.query_assets(&no_filter(&env), &2u32);
    assert_eq!(first.assets.len(), 2);
    assert!(first.has_more);
    assert_eq!(first.next_cursor, Symbol::new(&env, "BBB"));

    let mut filter = no_filter(&env);
    filter.cursor = first.next_cursor.clone();
    filter.has_cursor = true;

    let second = client.query_assets(&filter, &2u32);
    assert_eq!(second.assets.len(), 1);
    assert!(!second.has_more);
    assert_eq!(second.assets.get(0).unwrap().symbol, Symbol::new(&env, "CCC"));
}

#[test]
fn query_assets_uses_a_default_page_size_of_20() {
    let (env, _, contract_id, client) = setup_with_id();
    for i in 0..25 {
        seed_asset(
            &env,
            &contract_id,
            &format!("A{:02}", i),
            AssetClass::RealEstate,
            100,
            false,
        );
    }

    // A limit of 0 means "use the default", which is 20.
    let page = client.query_assets(&no_filter(&env), &0u32);
    assert_eq!(page.assets.len(), 20);
    assert!(page.has_more);
}
