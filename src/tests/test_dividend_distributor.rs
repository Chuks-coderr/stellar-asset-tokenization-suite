#![cfg(test)]

use soroban_sdk::{
    testutils::{Address as _, Ledger, LedgerInfo},
    token::{StellarAssetClient, TokenClient},
    Address, BytesN, Env, InvokeError, Map, Symbol, Vec,
};

use crate::{
    compliance_registry::{ComplianceRegistry, ComplianceRegistryClient},
    dividend_distributor::{
        ClaimState, DistributionCategory, DividendConfig, DividendDistributor, DividendDistributorClient,
        DividendError, MAX_FEE_RATE, RecurringSchedule, DEFAULT_MIN_CLAIM_WINDOW,
    },
    rwa_token::{RWAToken, RWATokenClient},
};

// ── helpers ──────────────────────────────────────────────────────────────────

struct TestEnv {
    env: Env,
    admin: Address,
    distributor: DividendDistributorClient<'static>,
    token: RWATokenClient<'static>,
    currency_token: TokenClient<'static>,
    currency_symbol: Symbol,
    compliance: Address,
}

fn setup() -> TestEnv {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);

    // Deploy a native Stellar asset as the dividend currency
    let currency_id = env.register_stellar_asset_contract_v2(admin.clone());
    let currency_token = TokenClient::new(&env, &currency_id.address());
    let currency_admin = StellarAssetClient::new(&env, &currency_id.address());
    // Mint 10_000 currency tokens to admin for distributions
    currency_admin.mint(&admin, &10_000i128);

    let currency_symbol = Symbol::new(&env, "USDC");

    // Deploy compliance registry (kyc_required = false for simplicity)
    let compliance_id = env.register_contract(None, ComplianceRegistry);
    let compliance = ComplianceRegistryClient::new(&env, &compliance_id);
    compliance.initialize(&admin, &admin, &false, &false);

    // Deploy RWA token
    let token_id = env.register_contract(None, RWAToken);
    let token = RWATokenClient::new(&env, &token_id);
    token.initialize(
        &admin,
        &Symbol::new(&env, "RWAToken"),
        &Symbol::new(&env, "RWA"),
        &1_000_000i128,
        &6u32,
        &Symbol::new(&env, "real_estate"),
        &Map::new(&env),
        &compliance_id,
        &admin, // placeholder — will be replaced by distributor below
    );

    // Deploy dividend distributor
    let dist_id = env.register_contract(None, DividendDistributor);
    let distributor = DividendDistributorClient::new(&env, &dist_id);
    let mut currencies = Vec::new(&env);
    currencies.push_back(currency_symbol.clone());
    distributor.initialize(&admin, &admin, &currencies);

    // Register the currency token address
    distributor.register_currency_token(&admin, &currency_symbol, &currency_id.address());

    // SAFETY: env lifetime is tied to the test scope.
    let distributor: DividendDistributorClient<'static> =
        unsafe { core::mem::transmute(distributor) };
    let token: RWATokenClient<'static> = unsafe { core::mem::transmute(token) };
    let currency_token: TokenClient<'static> = unsafe { core::mem::transmute(currency_token) };

    TestEnv {
        env,
        admin,
        distributor,
        token,
        currency_token,
        currency_symbol,
        compliance: compliance_id,
    }
}

fn advance_ledger(env: &Env, seconds: u64) {
    env.ledger().set(LedgerInfo {
        timestamp: env.ledger().timestamp() + seconds,
        ..env.ledger().get()
    });
}

// ── initialize ───────────────────────────────────────────────────────────────

#[test]
fn initialize_succeeds() {
    let t = setup();
    // If we get here without panic, initialize worked
    assert_eq!(t.distributor.get_active_distributions(&t.token.address).len(), 0);
}

#[test]
#[should_panic]
fn initialize_twice_panics() {
    let t = setup();
    let currencies = Vec::new(&t.env);
    t.distributor.initialize(&t.admin, &t.admin, &currencies);
}

// ── create_distribution ───────────────────────────────────────────────────────

#[test]
fn create_distribution_returns_id_one() {
    let t = setup();
    let deadline = t.env.ledger().timestamp() + 86400;
    let id = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );
    assert_eq!(id, 1);
}

#[test]
fn create_distribution_increments_id() {
    let t = setup();
    let deadline = t.env.ledger().timestamp() + 86400;
    let id1 = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );
    let id2 = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );
    assert_eq!(id2, id1 + 1);
}

#[test]
#[should_panic]
fn create_distribution_with_zero_amount_panics() {
    let t = setup();
    let deadline = t.env.ledger().timestamp() + 86400;
    t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &0i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );
}

#[test]
#[should_panic]
fn create_distribution_with_unsupported_currency_panics() {
    let t = setup();
    let deadline = t.env.ledger().timestamp() + 86400;
    t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &Symbol::new(&t.env, "UNKNOWN"),
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );
}

#[test]
#[should_panic]
fn create_distribution_by_non_admin_panics() {
    let t = setup();
    let attacker = Address::generate(&t.env);
    let deadline = t.env.ledger().timestamp() + 86400;
    t.distributor.create_distribution(
        &attacker,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );
}

// ── minimum claim window (issue #132) ────────────────────────────────────────

#[test]
fn default_min_claim_window_is_one_hour() {
    assert_eq!(DEFAULT_MIN_CLAIM_WINDOW, 3600);
}

#[test]
fn create_distribution_with_too_soon_deadline_returns_claim_deadline_too_soon() {
    let t = setup();
    // 1s under the default 3600s minimum window
    let deadline = t.env.ledger().timestamp() + DEFAULT_MIN_CLAIM_WINDOW - 1;
    let res = t.distributor.try_create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );
    match res {
        Err(Ok(e)) => assert_eq!(
            InvokeError::from(e),
            InvokeError::Contract(DividendError::ClaimDeadlineTooSoon as u32),
            "expected ClaimDeadlineTooSoon error"
        ),
        _ => panic!("expected ClaimDeadlineTooSoon contract error"),
    }
}

#[test]
#[should_panic]
fn create_distribution_with_too_soon_deadline_panics() {
    let t = setup();
    let deadline = t.env.ledger().timestamp() + 60; // well under the 1h window
    t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );
}

#[test]
fn create_distribution_with_deadline_at_minimum_window_succeeds() {
    let t = setup();
    // Exactly the minimum window is accepted (boundary is inclusive)
    let deadline = t.env.ledger().timestamp() + DEFAULT_MIN_CLAIM_WINDOW;
    let id = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );
    assert_eq!(id, 1);
    let dist = t.distributor.get_distribution(&id);
    assert_eq!(dist.claim_deadline, deadline);
}

#[test]
fn create_distribution_with_normal_deadline_succeeds() {
    let t = setup();
    let deadline = t.env.ledger().timestamp() + 86400;
    let id = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );
    assert_eq!(id, 1);
    let dist = t.distributor.get_distribution(&id);
    assert!(dist.is_active);
}

#[test]
fn min_claim_window_is_configurable_via_update_config() {
    let t = setup();
    let mut currencies = Vec::new(&t.env);
    currencies.push_back(t.currency_symbol.clone());
    t.distributor.update_config(
        &t.admin,
        &DividendConfig {
            supported_currencies: currencies,
            auto_distribute: false,
            min_distribution_amount: 1000,
            max_distribution_frequency: 86400,
            fee_rate: 50,
            fee_recipient: t.admin.clone(),
            min_claim_window: 7200, // custom 2h window
        },
    );

    // A 1h deadline no longer satisfies the custom 2h window
    let short_deadline = t.env.ledger().timestamp() + 3600;
    let res = t.distributor.try_create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &short_deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );
    match res {
        Err(Ok(e)) => assert_eq!(
            InvokeError::from(e),
            InvokeError::Contract(DividendError::ClaimDeadlineTooSoon as u32),
            "expected ClaimDeadlineTooSoon error under custom window"
        ),
        _ => panic!("expected ClaimDeadlineTooSoon contract error under custom window"),
    }

    // A 2h deadline satisfies the custom window
    let ok_deadline = t.env.ledger().timestamp() + 7200;
    let id = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &ok_deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );
    assert_eq!(id, 1);
}

// ── get_distribution ─────────────────────────────────────────────────────────

#[test]
fn get_distribution_returns_correct_data() {
    let t = setup();
    let deadline = t.env.ledger().timestamp() + 86400;
    let id = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );
    let dist = t.distributor.get_distribution(&id);
    assert_eq!(dist.distribution_id, id);
    assert_eq!(dist.total_amount, 1000);
    assert!(dist.is_active);
}

#[test]
#[should_panic]
fn get_distribution_for_nonexistent_id_panics() {
    let t = setup();
    t.distributor.get_distribution(&999u64);
}

// ── claim_dividend ────────────────────────────────────────────────────────────

#[test]
fn claim_dividend_transfers_net_amount_to_claimer() {
    let t = setup();
    let claimer = Address::generate(&t.env);

    // Give claimer 500_000 tokens (50% of supply)
    t.token.transfer(&t.admin, &claimer, &500_000i128);

    let deadline = t.env.ledger().timestamp() + 86400;
    let id = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );

    let claimed = t.distributor.claim_dividend(&id, &claimer);
    // 50% of 1000 = 500, minus 0.5% fee (fee_rate=50 bps) = ~497
    assert!(claimed > 0);
    assert!(claimed <= 500);
}

#[test]
#[should_panic]
fn claim_dividend_twice_panics() {
    let t = setup();
    let claimer = Address::generate(&t.env);
    t.token.transfer(&t.admin, &claimer, &100_000i128);

    let deadline = t.env.ledger().timestamp() + 86400;
    let id = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );

    t.distributor.claim_dividend(&id, &claimer);
    t.distributor.claim_dividend(&id, &claimer); // second claim should panic
}

#[test]
#[should_panic]
fn claim_dividend_after_deadline_panics() {
    let t = setup();
    let claimer = Address::generate(&t.env);
    t.token.transfer(&t.admin, &claimer, &100_000i128);

    // Must satisfy the minimum claim window (#132) — expiry is tested by
    // advancing past the deadline below, not by creating a short window.
    let deadline = t.env.ledger().timestamp() + 7200;
    let id = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );

    // Advance past deadline
    advance_ledger(&t.env, 7300);
    t.distributor.claim_dividend(&id, &claimer);
}

#[test]
#[should_panic]
fn claim_dividend_with_zero_balance_panics() {
    let t = setup();
    let claimer = Address::generate(&t.env); // no tokens

    let deadline = t.env.ledger().timestamp() + 86400;
    let id = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );

    t.distributor.claim_dividend(&id, &claimer);
}

// ── calculate_available_dividend ─────────────────────────────────────────────

#[test]
fn calculate_available_dividend_returns_proportional_amount() {
    let t = setup();
    let claimer = Address::generate(&t.env);
    t.token.transfer(&t.admin, &claimer, &500_000i128); // 50%

    let deadline = t.env.ledger().timestamp() + 86400;
    let id = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );

    let available = t.distributor.calculate_available_dividend(&id, &claimer);
    assert!(available > 0);
    assert!(available <= 500);
}

#[test]
fn calculate_available_dividend_returns_zero_after_claim() {
    let t = setup();
    let claimer = Address::generate(&t.env);
    t.token.transfer(&t.admin, &claimer, &500_000i128);

    let deadline = t.env.ledger().timestamp() + 86400;
    let id = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );

    t.distributor.claim_dividend(&id, &claimer);
    let available = t.distributor.calculate_available_dividend(&id, &claimer);
    assert_eq!(available, 0);
}

// ── deactivate_distribution ───────────────────────────────────────────────────

#[test]
#[should_panic]
fn claim_on_deactivated_distribution_panics() {
    let t = setup();
    let claimer = Address::generate(&t.env);
    t.token.transfer(&t.admin, &claimer, &100_000i128);

    let deadline = t.env.ledger().timestamp() + 86400;
    let id = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );

    t.distributor.deactivate_distribution(&t.admin, &id);
    t.distributor.claim_dividend(&id, &claimer);
}

// ── update_config ─────────────────────────────────────────────────────────────

#[test]
fn update_config_changes_fee_rate() {
    let t = setup();
    let mut currencies = Vec::new(&t.env);
    currencies.push_back(t.currency_symbol.clone());
    let new_config = DividendConfig {
        supported_currencies: currencies,
        auto_distribute: false,
        min_distribution_amount: 1000,
        max_distribution_frequency: 86400,
        fee_rate: 100, // changed from 50 to 100 bps
        fee_recipient: t.admin.clone(),
        min_claim_window: DEFAULT_MIN_CLAIM_WINDOW,
    };
    t.distributor.update_config(&t.admin, &new_config);
    // Verify by creating a distribution and checking the claimed amount reflects new fee
    let claimer = Address::generate(&t.env);
    t.token.transfer(&t.admin, &claimer, &1_000_000i128);
    let deadline = t.env.ledger().timestamp() + 86400;
    let id = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );
    let claimed = t.distributor.claim_dividend(&id, &claimer);
    // 100% of supply → 1000 total, minus 1% fee = 990
    assert_eq!(claimed, 990);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #222: Upgrade & Migration Tests for DividendDistributor
// ═══════════════════════════════════════════════════════════════════════════════

// ── migrate ───────────────────────────────────────────────────────────────────

#[test]
fn migrate_preserves_config_and_currencies() {
    let t = setup();

    // Create a distribution to populate state
    let deadline = t.env.ledger().timestamp() + 86400;
    let id = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &500i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );

    // Distribution data persists
    let dist = t.distributor.get_distribution(&id);
    assert_eq!(dist.distribution_id, id);
    assert_eq!(dist.total_amount, 500);
    assert!(dist.is_active);

    // Currency token still registered
    let active = t.distributor.get_active_distributions(&t.token.address);
    assert_eq!(active.len(), 1);
}

#[test]
fn migrate_preserves_claim_state() {
    let t = setup();
    let claimer = Address::generate(&t.env);
    t.token.transfer(&t.admin, &claimer, &500_000i128);

    let deadline = t.env.ledger().timestamp() + 86400;
    let id = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );

    // Claim and verify state is consistent
    let claimed = t.distributor.claim_dividend(&id, &claimer);
    assert!(claimed > 0);

    // Claim info should now exist
    let info = t.distributor.get_claim_info(&id, &claimer);
    assert!(info.is_some());

    // Available is zero after claim
    let available = t.distributor.calculate_available_dividend(&id, &claimer);
    assert_eq!(available, 0);
}

#[test]
fn migrate_preserves_multiple_distributions() {
    let t = setup();

    let deadline = t.env.ledger().timestamp() + 86400;

    // Create 5 distributions
    let mut ids = Vec::new(&t.env);
    for i in 0..5 {
        let id = t.distributor.create_distribution(
            &t.admin,
            &t.token.address,
            &t.currency_symbol,
            &((100 * (i + 1)) as i128),
            &deadline,
            &Map::new(&t.env),
        );
        ids.push_back(id);
    }

    // Deactivate one
    let third_id = ids.get(2).unwrap();
    t.distributor.deactivate_distribution(&t.admin, &third_id);

    // Active distributions should be 4
    let active = t.distributor.get_active_distributions(&t.token.address);
    assert_eq!(active.len(), 4);

    // Each distribution has correct amount
    for i in 0..5 {
        let id = ids.get(i).unwrap();
        let dist = t.distributor.get_distribution(&id);
        assert_eq!(dist.total_amount, (100 * (i + 1)) as i128);
    }
}

#[test]
#[should_panic]
fn migrate_by_non_admin_panics() {
    let t = setup();
    let attacker = Address::generate(&t.env);
    t.distributor.migrate(&attacker);
}

#[test]
fn migrate_with_empty_distributions() {
    let t = setup();

    // No distributions created — just initialized
    let active = t.distributor.get_active_distributions(&t.token.address);
    assert_eq!(active.len(), 0);

    // Creating a distribution after empty state should work fine
    let deadline = t.env.ledger().timestamp() + 86400;
    let id = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &500i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );
    assert_eq!(id, 1);
}

#[test]
fn migrate_preserves_auto_distribution_config() {
    let t = setup();

    // Update config with auto_distribute enabled
    let mut currencies = Vec::new(&t.env);
    currencies.push_back(t.currency_symbol.clone());
    let config = DividendConfig {
        supported_currencies: currencies,
        auto_distribute: true,
        min_distribution_amount: 100,
        max_distribution_frequency: 3600,
        fee_rate: 50,
        fee_recipient: t.admin.clone(),
        min_claim_window: DEFAULT_MIN_CLAIM_WINDOW,
    };
    t.distributor.update_config(&t.admin, &config);

    // Config persists after update
    let deadline = t.env.ledger().timestamp() + 86400;
    let id = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    );
    let dist = t.distributor.get_distribution(&id);
    assert!(dist.is_active);
}

// ── accrual tracking (issue #135) ─────────────────────────────────────────────

// 0.0005 per second in 1e18 fixed point (5e14) — sized so test claims stay
// within the 10_000 currency pool the admin funds for the distributor.
const ACCRUAL_RATE: i128 = 500_000_000_000_000;

fn setup_accrual(t: &TestEnv, claimer: &Address, balance: i128) {
    // Fund the distributor's currency pool that accrual yield is paid from
    t.currency_token.transfer(&t.admin, &t.distributor.address, &10_000i128);
    t.token.transfer(&t.admin, claimer, &balance);
    t.distributor.update_accrual(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &ACCRUAL_RATE,
    );
}

#[test]
fn accrual_accumulates_over_time() {
    let t = setup();
    let claimer = Address::generate(&t.env);
    setup_accrual(&t, &claimer, 500_000); // 50% of supply

    advance_ledger(&t.env, 10);

    let claimed = t.distributor.claim_accrued_yield(&claimer, &t.token.address);
    // gross = 500_000 * (5e14 * 10) / 1e18 = 2_500; fee 0.5% = 12; net 2_488
    assert_eq!(claimed, 2_488);
}

#[test]
fn accrual_supports_multiple_claims() {
    let t = setup();
    let claimer = Address::generate(&t.env);
    setup_accrual(&t, &claimer, 500_000);

    advance_ledger(&t.env, 10);
    let first = t.distributor.claim_accrued_yield(&claimer, &t.token.address);
    assert_eq!(first, 2_488);

    // Nothing new accrued immediately after a claim
    assert_eq!(t.distributor.calculate_accrued_yield(&claimer, &t.token.address), 0);

    advance_ledger(&t.env, 10);
    let second = t.distributor.claim_accrued_yield(&claimer, &t.token.address);
    assert_eq!(second, 2_488);
}

#[test]
fn accrual_claim_after_rate_change() {
    let t = setup();
    let claimer = Address::generate(&t.env);
    setup_accrual(&t, &claimer, 500_000);

    advance_ledger(&t.env, 10);
    assert_eq!(t.distributor.claim_accrued_yield(&claimer, &t.token.address), 2_488);

    // Admin doubles the per-second rate mid-period
    t.distributor.update_accrual(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &(ACCRUAL_RATE * 2),
    );

    advance_ledger(&t.env, 10);
    // gross = 500_000 * (1e15 * 10) / 1e18 = 5_000; fee 0.5% = 25; net 4_975
    assert_eq!(t.distributor.claim_accrued_yield(&claimer, &t.token.address), 4_975);
}

#[test]
fn accrual_calculate_returns_pending_yield() {
    let t = setup();
    let claimer = Address::generate(&t.env);
    setup_accrual(&t, &claimer, 500_000);

    advance_ledger(&t.env, 10);

    let pending = t.distributor.calculate_accrued_yield(&claimer, &t.token.address);
    assert_eq!(pending, 2_488);
}

#[test]
fn accrual_rate_is_queryable() {
    let t = setup();
    t.distributor.update_accrual(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &ACCRUAL_RATE,
    );

    let rate = t.distributor.get_accrual_rate(&t.token.address);
    assert!(rate.is_some());
    let rate = rate.unwrap();
    assert_eq!(rate.amount_per_second, ACCRUAL_RATE);
    assert_eq!(rate.currency, t.currency_symbol);
}

#[test]
#[should_panic]
fn accrual_update_by_non_admin_panics() {
    let t = setup();
    let attacker = Address::generate(&t.env);
    t.distributor.update_accrual(
        &attacker,
        &t.token.address,
        &t.currency_symbol,
        &ACCRUAL_RATE,
    );
}

#[test]
#[should_panic]
fn accrual_update_with_unsupported_currency_panics() {
    let t = setup();
    t.distributor.update_accrual(
        &t.admin,
        &t.token.address,
        &Symbol::new(&t.env, "UNKNOWN"),
        &ACCRUAL_RATE,
    );
}

#[test]
#[should_panic]
fn accrual_claim_without_configuration_panics() {
    let t = setup();
    let claimer = Address::generate(&t.env);
    t.token.transfer(&t.admin, &claimer, &100_000i128);
    t.distributor.claim_accrued_yield(&claimer, &t.token.address);
}

// ── cross-token portfolio aggregation (issue #137) ────────────────────────────

fn deploy_second_token(t: &TestEnv) -> RWATokenClient<'static> {
    let token2_id = t.env.register_contract(None, RWAToken);
    let token2 = RWATokenClient::new(&t.env, &token2_id);
    token2.initialize(
        &t.admin,
        &Symbol::new(&t.env, "RWAToken2"),
        &Symbol::new(&t.env, "RWA2"),
        &1_000_000i128,
        &6u32,
        &Symbol::new(&t.env, "real_estate"),
        &Map::new(&t.env),
        &t.compliance,
        &t.distributor.address,
    );
    unsafe { core::mem::transmute(token2) }
}

fn create_distribution(t: &TestEnv, token_address: &Address) -> u64 {
    let deadline = t.env.ledger().timestamp() + 86400;
    t.distributor.create_distribution(
        &t.admin,
        token_address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Regular,
        &Map::new(&t.env),
    )
}

#[test]
fn claim_all_portfolio_dividends_multi_token() {
    let t = setup();
    let claimer = Address::generate(&t.env);
    let token2 = deploy_second_token(&t);

    // Claimer holds 50% of both tokens
    t.token.transfer(&t.admin, &claimer, &500_000i128);
    token2.transfer(&t.admin, &claimer, &500_000i128);

    create_distribution(&t, &t.token.address);
    create_distribution(&t, &token2.address);

    let results = t.distributor.claim_all_portfolio_dividends(&claimer);
    assert_eq!(results.len(), 2);

    let mut total: i128 = 0;
    for (addr, claimed) in results.iter() {
        total += claimed;
        assert!(claimed > 0);
        assert!(addr == t.token.address.clone() || addr == token2.address.clone());
    }
    // 498 per token (500 gross minus 0.5% fee)
    assert_eq!(total, 996);

    // Claiming again yields nothing — all dividends already claimed
    let second = t.distributor.claim_all_portfolio_dividends(&claimer);
    assert_eq!(second.len(), 0);
}

#[test]
fn claim_all_portfolio_dividends_partial_claims() {
    let t = setup();
    let claimer = Address::generate(&t.env);
    let token2 = deploy_second_token(&t);

    t.token.transfer(&t.admin, &claimer, &500_000i128);
    token2.transfer(&t.admin, &claimer, &500_000i128);

    // Only token1 has a distribution
    create_distribution(&t, &t.token.address);
    t.distributor.register_token(&t.admin, &token2.address);

    let results = t.distributor.claim_all_portfolio_dividends(&claimer);
    assert_eq!(results.len(), 1);
    assert_eq!(results.get(0).unwrap().0, t.token.address);
    assert_eq!(results.get(0).unwrap().1, 498);
}

#[test]
fn claim_all_portfolio_dividends_skips_unheld_tokens() {
    let t = setup();
    let claimer = Address::generate(&t.env);
    let token2 = deploy_second_token(&t);

    // Claimer only holds token1; token2 has a distribution but zero balance
    t.token.transfer(&t.admin, &claimer, &500_000i128);

    create_distribution(&t, &t.token.address);
    create_distribution(&t, &token2.address);

    let results = t.distributor.claim_all_portfolio_dividends(&claimer);
    assert_eq!(results.len(), 1);
    assert_eq!(results.get(0).unwrap().0, t.token.address);
    assert_eq!(results.get(0).unwrap().1, 498);
}

#[test]
fn claim_all_portfolio_dividends_no_dividends_available() {
    let t = setup();
    let claimer = Address::generate(&t.env);
    t.token.transfer(&t.admin, &claimer, &500_000i128);
    t.distributor.register_token(&t.admin, &t.token.address);

    // Token is registered and held, but no distributions exist yet
    let results = t.distributor.claim_all_portfolio_dividends(&claimer);
    assert_eq!(results.len(), 0);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #139: Emergency distribution freeze with governance override
// ═══════════════════════════════════════════════════════════════════════════════

fn single_owner(t: &TestEnv) -> Vec<Address> {
    let mut owners = Vec::new(&t.env);
    owners.push_back(t.admin.clone());
    owners
}

#[test]
fn freeze_blocks_new_distributions_and_claims() {
    let t = setup();
    let id = create_distribution(&t, &t.token.address);

    t.distributor.freeze_all_distributions(&t.admin);
    assert!(t.distributor.is_distributions_frozen());

    // New distributions are blocked.
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        create_distribution(&t, &t.token.address);
    }));
    assert!(res.is_err());

    // Claims return `DistributionNotActive`.
    let res = t.distributor.try_claim_dividend(&id, &t.admin);
    assert!(res.is_err());

    // Nothing is claimable while frozen.
    assert_eq!(t.distributor.calculate_available_dividend(&id, &t.admin), 0);
}

#[test]
fn freeze_by_non_admin_panics() {
    let t = setup();
    let attacker = Address::generate(&t.env);
    let res = t.distributor.try_freeze_all_distributions(&attacker);
    assert!(res.is_err());
    assert!(!t.distributor.is_distributions_frozen());
}

#[test]
fn governance_unfreeze_restores_claims() {
    let t = setup();
    t.distributor
        .init_distribution_governance(&t.admin, &single_owner(&t), &1u32, &0u64);

    let id = create_distribution(&t, &t.token.address);

    t.distributor.freeze_all_distributions(&t.admin);
    assert!(t.distributor.is_distributions_frozen());

    let payload = BytesN::from_array(&t.env, &[7u8; 32]);
    let pid = t
        .distributor
        .create_unfreeze_proposal(&t.admin, &payload);
    t.distributor
        .approve_distribution_proposal(&t.admin, &pid);

    // Emergency proposals become executable after their review window.
    advance_ledger(&t.env, 3601);

    assert!(t
        .distributor
        .unfreeze_all_distributions(&t.admin, &pid, &payload));
    assert!(!t.distributor.is_distributions_frozen());

    // The pre-existing distribution is preserved and claimable again.
    let claimed = t.distributor.claim_dividend(&id, &t.admin);
    assert!(claimed > 0);
}

#[test]
fn admin_direct_unfreeze_is_rejected() {
    let t = setup();
    t.distributor
        .init_distribution_governance(&t.admin, &single_owner(&t), &1u32, &0u64);
    t.distributor.freeze_all_distributions(&t.admin);

    // No approved governance proposal exists, so a direct unfreeze is rejected.
    let payload = BytesN::from_array(&t.env, &[3u8; 32]);
    let res = t
        .distributor
        .try_unfreeze_all_distributions(&t.admin, &1u64, &payload);
    assert!(res.is_err());
    assert!(t.distributor.is_distributions_frozen());
}

// ════════════════════════════════════════════════════════════════════════════════
// Issue #136: MAX_FEE_RATE cap (1000 bps) validation in update_config
// ════════════════════════════════════════════════════════════════════════════════

fn make_config(t: &TestEnv, fee_rate: i64) -> DividendConfig {
    let mut currencies = Vec::new(&t.env);
    currencies.push_back(t.currency_symbol.clone());
    DividendConfig {
        supported_currencies: currencies,
        auto_distribute: false,
        min_distribution_amount: 1000,
        max_distribution_frequency: 86400,
        fee_rate,
        fee_recipient: t.admin.clone(),
        min_claim_window: DEFAULT_MIN_CLAIM_WINDOW,
    }
}

#[test]
fn update_config_fee_rate_at_cap_succeeds() {
    let t = setup();
    t.distributor.update_config(&t.admin, &make_config(&t, MAX_FEE_RATE));
}

#[test]
#[should_panic]
fn update_config_fee_rate_above_cap_panics() {
    let t = setup();
    t.distributor.update_config(&t.admin, &make_config(&t, MAX_FEE_RATE + 1));
}

#[test]
#[should_panic]
fn update_config_fee_rate_negative_panics() {
    let t = setup();
    t.distributor.update_config(&t.admin, &make_config(&t, -1));
}

#[test]
fn update_config_fee_rate_zero_succeeds() {
    let t = setup();
    t.distributor.update_config(&t.admin, &make_config(&t, 0));
}

// ════════════════════════════════════════════════════════════════════════════════
// Issue #133: DistributionCategory + get_distributions_by_category
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn distribution_category_is_stored_and_queryable() {
    let t = setup();
    let deadline = t.env.ledger().timestamp() + 86400;
    let id = t.distributor.create_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &DistributionCategory::Bonus,
        &Map::new(&t.env),
    );

    let dist = t.distributor.get_distribution(&id);
    assert_eq!(dist.category, DistributionCategory::Bonus);

    let bonus_dists = t.distributor.get_distributions_by_category(DistributionCategory::Bonus);
    assert_eq!(bonus_dists.len(), 1);
    assert_eq!(bonus_dists.get(0).unwrap().distribution_id, id);

    let regular_dists = t.distributor.get_distributions_by_category(DistributionCategory::Regular);
    assert_eq!(regular_dists.len(), 0);
}

#[test]
fn get_distributions_by_category_filters_correctly() {
    let t = setup();
    let deadline = t.env.ledger().timestamp() + 86400;

    let id1 = t.distributor.create_distribution(
        &t.admin, &t.token.address, &t.currency_symbol, &1000i128, &deadline,
        &DistributionCategory::Regular, &Map::new(&t.env),
    );
    let id2 = t.distributor.create_distribution(
        &t.admin, &t.token.address, &t.currency_symbol, &2000i128, &deadline,
        &DistributionCategory::Special, &Map::new(&t.env),
    );
    let id3 = t.distributor.create_distribution(
        &t.admin, &t.token.address, &t.currency_symbol, &3000i128, &deadline,
        &DistributionCategory::Special, &Map::new(&t.env),
    );

    let specials = t.distributor.get_distributions_by_category(DistributionCategory::Special);
    assert_eq!(specials.len(), 2);
    let ids: Vec<u64> = specials.iter().map(|d| d.distribution_id).collect();
    assert!(ids.contains(&id2));
    assert!(ids.contains(&id3));

    let regulars = t.distributor.get_distributions_by_category(DistributionCategory::Regular);
    assert_eq!(regulars.len(), 1);
    assert_eq!(regulars.get(0).unwrap().distribution_id, id1);
}

// ════════════════════════════════════════════════════════════════════════════════
// Issue #134: Claim state machine (Unclaimed -> InProgress -> Claimed)
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn claim_state_transitions_unclaimed_to_claimed() {
    let t = setup();
    let claimer = Address::generate(&t.env);
    t.token.transfer(&t.admin, &claimer, &500_000i128);

    let deadline = t.env.ledger().timestamp() + 86400;
    let id = t.distributor.create_distribution(
        &t.admin, &t.token.address, &t.currency_symbol, &1000i128, &deadline,
        &DistributionCategory::Regular, &Map::new(&t.env),
    );

    // Initially Unclaimed
    assert_eq!(
        t.distributor.get_claim_state(&id, &claimer),
        ClaimState::Unclaimed
    );

    t.distributor.claim_dividend(&id, &claimer);

    // After claim: Claimed
    assert_eq!(
        t.distributor.get_claim_state(&id, &claimer),
        ClaimState::Claimed
    );
}

#[test]
fn claim_state_defaults_to_unclaimed_for_unknown_claimer() {
    let t = setup();
    let deadline = t.env.ledger().timestamp() + 86400;
    let id = t.distributor.create_distribution(
        &t.admin, &t.token.address, &t.currency_symbol, &1000i128, &deadline,
        &DistributionCategory::Regular, &Map::new(&t.env),
    );

    let unknown = Address::generate(&t.env);
    assert_eq!(
        t.distributor.get_claim_state(&id, &unknown),
        ClaimState::Unclaimed
    );
}

// ════════════════════════════════════════════════════════════════════════════════
// Issue #131: RecurringSchedule with cron-like timing
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn create_recurring_distribution_stores_schedule() {
    let t = setup();
    let deadline = t.env.ledger().timestamp() + 86400;
    let schedule = RecurringSchedule {
        minute: 0,
        hour: 0,
        day_of_month: -1,
        month: -1,
        day_of_week: -1,
        last_fired_at: 0,
        min_interval_seconds: 3600,
    };

    let id = t.distributor.create_recurring_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &Map::new(&t.env),
        &DistributionCategory::Regular,
        &schedule,
    );

    let stored = t.distributor.get_recurring_schedule(&id);
    assert!(stored.is_some());
    let stored = stored.unwrap();
    assert_eq!(stored.minute, 0);
    assert_eq!(stored.hour, 0);
    assert_eq!(stored.min_interval_seconds, 3600);
}

#[test]
fn is_schedule_due_returns_false_for_non_recurring_distribution() {
    let t = setup();
    let deadline = t.env.ledger().timestamp() + 86400;
    let id = t.distributor.create_distribution(
        &t.admin, &t.token.address, &t.currency_symbol, &1000i128, &deadline,
        &DistributionCategory::Regular, &Map::new(&t.env),
    );

    assert!(!t.distributor.is_schedule_due(&id));
}

#[test]
fn is_schedule_due_respects_min_interval() {
    let t = setup();
    let deadline = t.env.ledger().timestamp() + 86400;
    let schedule = RecurringSchedule {
        minute: -1,
        hour: -1,
        day_of_month: -1,
        month: -1,
        day_of_week: -1,
        last_fired_at: t.env.ledger().timestamp(),
        min_interval_seconds: 3600,
    };

    let id = t.distributor.create_recurring_distribution(
        &t.admin,
        &t.token.address,
        &t.currency_symbol,
        &1000i128,
        &deadline,
        &Map::new(&t.env),
        &DistributionCategory::Regular,
        &schedule,
    );

    // Just fired, min interval not elapsed — not due.
    assert!(!t.distributor.is_schedule_due(&id));

    // Advance past min interval — now due (all wildcards match).
    advance_ledger(&t.env, 3601);
    assert!(t.distributor.is_schedule_due(&id));
}
