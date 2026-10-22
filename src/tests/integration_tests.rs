#![cfg(test)]

use soroban_sdk::{
    testutils::{Address as _, AuthorizedFunction, AuthorizedInvocation, Ledger},
    Address, BytesN, Env, Map, Symbol, Vec,
};

use crate::{
    asset_factory::{
        AssetClass, AssetConfig, AssetFactory, AssetFactoryClient, AssetTemplate, ComplianceRules,
    },
    compliance_registry::{ComplianceRegistry, ComplianceRegistryClient, KYCStatus},
    custody_validator::{CustodyAttestation, CustodyValidator, CustodyValidatorClient},
    dividend_distributor::{DividendDistributor, DividendDistributorClient},
    rwa_token::{RWAToken, RWATokenClient},
    secondary_market::{SecondaryMarket, SecondaryMarketClient},
    RwaDeploySpec,
};

// ── Integration Test Environment Setup ───────────────────────────────────────

struct IntegrationTestEnv {
    env: Env,
    admin: Address,
    user1: Address,
    user2: Address,
    asset_factory: AssetFactoryClient<'static>,
    compliance_registry: ComplianceRegistryClient<'static>,
    dividend_distributor: DividendDistributorClient<'static>,
    secondary_market: SecondaryMarketClient<'static>,
    rwa_token: RWATokenClient<'static>,
    custody_validator: CustodyValidatorClient<'static>,
    oracle1: Address,
    base_currency: Address,
}

fn setup_integration_test() -> IntegrationTestEnv {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let user1 = Address::generate(&env);
    let user2 = Address::generate(&env);
    let oracle1 = Address::generate(&env);

    // Deploy Compliance Registry
    let compliance_id = env.register_contract(None, ComplianceRegistry);
    let compliance_registry = ComplianceRegistryClient::new(&env, &compliance_id);
    compliance_registry.initialize(&admin, &admin, &true, &true);

    // The registry requires KYC for both sides of a transfer. Give the
    // fixture participants valid KYC by default; individual tests can
    // override a participant's status to exercise rejection paths.
    let default_kyc = KYCStatus {
        is_verified: true,
        verification_level: 2,
        expiry_date: env.ledger().timestamp() + 86400 * 365,
        jurisdiction: Symbol::new(&env, "US"),
        is_accredited: true,
        risk_score: 2,
        aml_flags: Vec::new(&env),
    };
    compliance_registry.update_kyc_status(&admin, &admin, &default_kyc.clone());
    compliance_registry.update_kyc_status(&admin, &user1, &default_kyc.clone());
    compliance_registry.update_kyc_status(&admin, &user2, &default_kyc);

    // Transfer restrictions are enabled in this fixture. Seed generous
    // default limits so ordinary transfers succeed; tests that exercise
    // limits overwrite the relevant participant's limits explicitly.
    let default_limits = crate::compliance_registry::TransferLimits {
        daily_limit: 1_000_000_000,
        monthly_limit: 1_000_000_000,
        annual_limit: 1_000_000_000,
        remaining_daily: 1_000_000_000,
        remaining_monthly: 1_000_000_000,
        remaining_annual: 1_000_000_000,
        last_reset_daily: 0,
        last_reset_monthly: 0,
        last_reset_annual: 0,
    };
    compliance_registry.set_transfer_limits(&admin, &admin, &default_limits);
    compliance_registry.set_transfer_limits(&admin, &user1, &default_limits);
    compliance_registry.set_transfer_limits(&admin, &user2, &default_limits);

    // Deploy Dividend Distributor
    let dividend_id = env.register_contract(None, DividendDistributor);
    let dividend_distributor = DividendDistributorClient::new(&env, &dividend_id);
    let supported_currencies = Vec::from_array(&env, [Symbol::new(&env, "USDC")]);
    dividend_distributor.initialize(&admin, &admin, &supported_currencies);

    // Deploy Asset Factory
    let factory_id = env.register_contract(None, AssetFactory);
    let asset_factory = AssetFactoryClient::new(&env, &factory_id);
    asset_factory.initialize(&admin, &admin);

    // Deploy Secondary Market
    let market_id = env.register_contract(None, SecondaryMarket);
    let secondary_market = SecondaryMarketClient::new(&env, &market_id);

    // Create a mock base currency token
    let base_currency = Address::generate(&env);

    secondary_market.initialize(
        &admin,
        &base_currency,
        &compliance_id,
        &dividend_id,
        &50i64,   // 0.5% fee
        &100i128, // min order size
        &2000i64, // 20% max price deviation
    );

    // Deploy Custody Validator
    let custody_id = env.register_contract(None, CustodyValidator);
    let custody_validator = CustodyValidatorClient::new(&env, &custody_id);
    let oracle1 = Address::generate(&env);
    custody_validator.initialize(&admin, &admin, &Vec::from_array(&env, [oracle1.clone()]));

    // Register the admin as an active custodian so attestation flows work.
    custody_validator.register_custodian(
        &admin,
        &admin,
        &Symbol::new(&env, "IntegrationCustodian"),
        &Symbol::new(&env, "US"),
        &Symbol::new(&env, "LIC_INTEGRATION"),
        &Vec::from_array(
            &env,
            [
                Symbol::new(&env, "real_estate"),
                Symbol::new(&env, "commodities"),
                Symbol::new(&env, "precious_metals"),
                Symbol::new(&env, "physical"),
            ],
        ),
        &0i128,
        &Symbol::new(&env, "TestInsurer"),
    );

    // Deploy RWA Token
    let token_id = env.register_contract(None, RWAToken);
    let rwa_token = RWATokenClient::new(&env, &token_id);

    // Register currency token for dividends
    dividend_distributor.register_currency_token(
        &admin,
        &Symbol::new(&env, "USDC"),
        &base_currency,
    );

    // SAFETY: env lifetime is tied to the test scope
    let asset_factory: AssetFactoryClient<'static> = unsafe { core::mem::transmute(asset_factory) };
    let compliance_registry: ComplianceRegistryClient<'static> =
        unsafe { core::mem::transmute(compliance_registry) };
    let dividend_distributor: DividendDistributorClient<'static> =
        unsafe { core::mem::transmute(dividend_distributor) };
    let secondary_market: SecondaryMarketClient<'static> =
        unsafe { core::mem::transmute(secondary_market) };
    let rwa_token: RWATokenClient<'static> = unsafe { core::mem::transmute(rwa_token) };
    let custody_validator: CustodyValidatorClient<'static> =
        unsafe { core::mem::transmute(custody_validator) };

    IntegrationTestEnv {
        env,
        admin,
        user1,
        user2,
        asset_factory,
        compliance_registry,
        dividend_distributor,
        secondary_market,
        rwa_token,
        custody_validator,
        oracle1,
        base_currency,
    }
}

// ── Integration Test 1: Complete Asset Lifecycle ────────────────────────────────

#[test]
fn test_complete_asset_lifecycle() {
    let t = setup_integration_test();

    // Step 1: Register template for Real Estate
    let wasm_hash = BytesN::from_array(&t.env, &[1u8; 32]);
    let compliance_rules = ComplianceRules {
        kyc_required: true,
        accredited_investor_only: false,
        geographic_restrictions: Vec::new(&t.env),
        holding_period_days: 0,
        transfer_limits: 1_000_000i128,
    };

    let asset_config = AssetConfig {
        name: Symbol::new(&t.env, "ManhattanProperty"),
        symbol: Symbol::new(&t.env, "MANH"),
        decimals: 6,
        total_supply: 1_000_000i128,
        asset_class: AssetClass::RealEstate,
        compliance_rules: compliance_rules.clone(),
        dividend_schedule: Vec::new(&t.env),
        metadata: Map::new(&t.env),
    };

    let template = AssetTemplate {
        asset_class: AssetClass::RealEstate,
        base_config: asset_config.clone(),
        wasm_hash: wasm_hash.clone(),
        is_active: true,
        version: 1,
    };

    t.asset_factory.register_template(&t.admin, &template);

    // Step 2: Initialize RWA Token with compliance and dividend distributor
    t.rwa_token.initialize(
        &t.admin,
        &Symbol::new(&t.env, "ManhattanProperty"),
        &Symbol::new(&t.env, "MANH"),
        &1_000_000i128,
        &6u32,
        &Symbol::new(&t.env, "real_estate"),
        &Map::new(&t.env),
        &t.compliance_registry.address.clone(),
        &t.dividend_distributor.address.clone(),
    );

    // Verify admin has initial supply
    let admin_balance = t.rwa_token.get_balance(&t.admin);
    assert_eq!(admin_balance.amount, 1_000_000);

    // Step 3: Set up KYC for users
    let kyc_status = KYCStatus {
        is_verified: true,
        verification_level: 2,
        expiry_date: t.env.ledger().timestamp() + 86400 * 365,
        jurisdiction: Symbol::new(&t.env, "US"),
        is_accredited: true,
        risk_score: 3,
        aml_flags: Vec::new(&t.env),
    };

    t.compliance_registry
        .update_kyc_status(&t.admin, &t.user1, &kyc_status.clone());
    t.compliance_registry
        .update_kyc_status(&t.admin, &t.user2, &kyc_status);

    // Step 4: Transfer tokens to user1
    t.rwa_token.transfer(&t.admin, &t.user1, &100_000i128);

    let user1_balance = t.rwa_token.get_balance(&t.user1);
    assert_eq!(user1_balance.amount, 100_000);

    let admin_balance = t.rwa_token.get_balance(&t.admin);
    assert_eq!(admin_balance.amount, 900_000);

    // Step 5: Create dividend distribution
    let distribution_id = t.dividend_distributor.create_distribution(
        &t.admin,
        &t.rwa_token.address.clone(),
        &Symbol::new(&t.env, "USDC"),
        &10_000i128,
        &(t.env.ledger().timestamp() + 86400 * 30),
        &Map::new(&t.env),
    );

    assert_eq!(distribution_id, 1);

    // Step 6: User1 claims dividend
    let claimed = t
        .dividend_distributor
        .claim_dividend(&distribution_id, &t.user1.clone());
    assert!(claimed > 0);

    // Step 7: Verify claim info
    let claim_info = t
        .dividend_distributor
        .get_claim_info(&distribution_id, &t.user1.clone());
    assert!(claim_info.is_some());
}

// ── Integration Test 2: Compliance-Enforced Trading ────────────────────────────

#[test]
fn test_compliance_enforced_trading() {
    let t = setup_integration_test();

    // Initialize RWA Token
    t.rwa_token.initialize(
        &t.admin,
        &Symbol::new(&t.env, "CommodityToken"),
        &Symbol::new(&t.env, "COMM"),
        &1_000_000i128,
        &6u32,
        &Symbol::new(&t.env, "commodity"),
        &Map::new(&t.env),
        &t.compliance_registry.address.clone(),
        &t.dividend_distributor.address.clone(),
    );

    // Set up KYC for user1 (verified)
    let kyc_verified = KYCStatus {
        is_verified: true,
        verification_level: 2,
        expiry_date: t.env.ledger().timestamp() + 86400 * 365,
        jurisdiction: Symbol::new(&t.env, "US"),
        is_accredited: true,
        risk_score: 3,
        aml_flags: Vec::new(&t.env),
    };
    t.compliance_registry
        .update_kyc_status(&t.admin, &t.user1, &kyc_verified);

    // Set up KYC for user2 (not verified)
    let kyc_unverified = KYCStatus {
        is_verified: false,
        verification_level: 0,
        expiry_date: 0,
        jurisdiction: Symbol::new(&t.env, "UNKNOWN"),
        is_accredited: false,
        risk_score: 5,
        aml_flags: Vec::new(&t.env),
    };
    t.compliance_registry
        .update_kyc_status(&t.admin, &t.user2, &kyc_unverified);

    // Transfer tokens to user1
    t.rwa_token.transfer(&t.admin, &t.user1, &100_000i128);

    // Transfer from verified user1 to unverified user2 should fail
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        t.rwa_token.transfer(&t.user1, &t.user2, &1000i128);
    }));
    assert!(result.is_err());

    // Now verify user2
    let kyc_verified2 = KYCStatus {
        is_verified: true,
        verification_level: 2,
        expiry_date: t.env.ledger().timestamp() + 86400 * 365,
        jurisdiction: Symbol::new(&t.env, "US"),
        is_accredited: true,
        risk_score: 3,
        aml_flags: Vec::new(&t.env),
    };
    t.compliance_registry
        .update_kyc_status(&t.admin, &t.user2, &kyc_verified2);

    // Transfer should now succeed
    t.rwa_token.transfer(&t.user1, &t.user2, &1000i128);

    let user2_balance = t.rwa_token.get_balance(&t.user2);
    assert_eq!(user2_balance.amount, 1000);
}

// ── Integration Test 3: Secondary Market Trading ───────────────────────────────

#[test]
fn test_secondary_market_trading() {
    let t = setup_integration_test();

    // Initialize RWA Token
    t.rwa_token.initialize(
        &t.admin,
        &Symbol::new(&t.env, "SecurityToken"),
        &Symbol::new(&t.env, "SECU"),
        &1_000_000i128,
        &6u32,
        &Symbol::new(&t.env, "security"),
        &Map::new(&t.env),
        &t.compliance_registry.address.clone(),
        &t.dividend_distributor.address.clone(),
    );

    // Set up KYC for both users
    let kyc_status = KYCStatus {
        is_verified: true,
        verification_level: 2,
        expiry_date: t.env.ledger().timestamp() + 86400 * 365,
        jurisdiction: Symbol::new(&t.env, "US"),
        is_accredited: true,
        risk_score: 3,
        aml_flags: Vec::new(&t.env),
    };

    t.compliance_registry
        .update_kyc_status(&t.admin, &t.user1, &kyc_status.clone());
    t.compliance_registry
        .update_kyc_status(&t.admin, &t.user2, &kyc_status);

    // Transfer tokens to user1 for selling
    t.rwa_token.transfer(&t.admin, &t.user1, &10_000i128);

    // Place a sell order
    let sell_order_id = t.secondary_market.place_order(
        &t.user1,
        &t.rwa_token.address.clone(),
        &Symbol::new(&t.env, "sell"),
        &100i128,                              // price
        &1000i128,                             // amount
        &(t.env.ledger().timestamp() + 86400), // expiry
        &100i128,                              // min_fill
    );

    assert_eq!(sell_order_id, 1);

    // Verify order was placed (check user1's balance decreased due to escrow)
    let user1_balance = t.rwa_token.get_balance(&t.user1);
    assert_eq!(user1_balance.amount, 9_000); // 10,000 - 1,000 escrowed

    // Fill the sell order (user2 buys)
    t.secondary_market
        .fill_order(&t.user2, &sell_order_id, &500i128);

    // Verify trade execution
    let user1_balance = t.rwa_token.get_balance(&t.user1);
    let user2_balance = t.rwa_token.get_balance(&t.user2);

    // User1 should have 9,500 (9,000 remaining + 500 unfilled returned)
    assert_eq!(user1_balance.amount, 9_500);
    // User2 should have 500 tokens
    assert_eq!(user2_balance.amount, 500);
}

// ── Integration Test 4: Blacklist Enforcement ────────────────────────────────────

#[test]
fn test_blacklist_enforcement() {
    let t = setup_integration_test();

    // Initialize RWA Token
    t.rwa_token.initialize(
        &t.admin,
        &Symbol::new(&t.env, "BlacklistTest"),
        &Symbol::new(&t.env, "BLKT"),
        &1_000_000i128,
        &6u32,
        &Symbol::new(&t.env, "real_estate"),
        &Map::new(&t.env),
        &t.compliance_registry.address.clone(),
        &t.dividend_distributor.address.clone(),
    );

    // Set up KYC for both users
    let kyc_status = KYCStatus {
        is_verified: true,
        verification_level: 2,
        expiry_date: t.env.ledger().timestamp() + 86400 * 365,
        jurisdiction: Symbol::new(&t.env, "US"),
        is_accredited: true,
        risk_score: 3,
        aml_flags: Vec::new(&t.env),
    };

    t.compliance_registry
        .update_kyc_status(&t.admin, &t.user1, &kyc_status.clone());
    t.compliance_registry
        .update_kyc_status(&t.admin, &t.user2, &kyc_status);

    // Transfer tokens to user1
    t.rwa_token.transfer(&t.admin, &t.user1, &100_000i128);

    // Blacklist user1
    t.compliance_registry.add_to_blacklist(
        &t.admin,
        &t.user1,
        &Symbol::new(&t.env, "suspicious_activity"),
    );

    // Transfer from blacklisted user should fail
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        t.rwa_token.transfer(&t.user1, &t.user2, &1000i128);
    }));
    assert!(result.is_err());

    // Remove from blacklist
    t.compliance_registry
        .remove_from_blacklist(&t.admin, &t.user1);

    // Transfer should now succeed
    t.rwa_token.transfer(&t.user1, &t.user2, &1000i128);

    let user2_balance = t.rwa_token.get_balance(&t.user2);
    assert_eq!(user2_balance.amount, 1000);
}

// ── Integration Test 5: Token Locking and Voting Power ─────────────────────────

#[test]
fn test_token_locking_and_voting_power() {
    let t = setup_integration_test();

    // Initialize RWA Token
    t.rwa_token.initialize(
        &t.admin,
        &Symbol::new(&t.env, "GovernanceToken"),
        &Symbol::new(&t.env, "GOV"),
        &1_000_000i128,
        &6u32,
        &Symbol::new(&t.env, "security"),
        &Map::new(&t.env),
        &t.compliance_registry.address.clone(),
        &t.dividend_distributor.address.clone(),
    );

    // Transfer tokens to user1
    t.rwa_token.transfer(&t.admin, &t.user1, &100_000i128);

    // Lock tokens for voting
    t.rwa_token
        .lock_tokens(&t.user1, &t.user1, &10_000i128, &3600u64);

    let balance = t.rwa_token.get_balance(&t.user1);
    assert_eq!(balance.amount, 100_000); // total holding is unchanged by locking
    assert_eq!(balance.locked_amount, 10_000);
    assert_eq!(balance.voting_power, 10_000);

    // Try to transfer more than the unlocked portion (100,000 - 10,000).
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        t.rwa_token.transfer(&t.user1, &t.user2, &95_000i128);
    }));
    assert!(result.is_err());

    // Transfer unlocked amount should succeed
    t.rwa_token.transfer(&t.user1, &t.user2, &5_000i128);

    let user2_balance = t.rwa_token.get_balance(&t.user2);
    assert_eq!(user2_balance.amount, 5_000);

    // Unlock tokens
    t.rwa_token.unlock_tokens(&t.user1, &t.user1, &10_000i128);

    let balance = t.rwa_token.get_balance(&t.user1);
    assert_eq!(balance.amount, 95_000); // 100,000 - 5,000 transferred
    assert_eq!(balance.locked_amount, 0);
    assert_eq!(balance.voting_power, 0);
}

// ── Integration Test 6: Multi-Currency Dividend Distribution ───────────────────

#[test]
fn test_multi_currency_dividend_distribution() {
    let t = setup_integration_test();

    // Initialize RWA Token
    t.rwa_token.initialize(
        &t.admin,
        &Symbol::new(&t.env, "MultiDivToken"),
        &Symbol::new(&t.env, "MULT"),
        &1_000_000i128,
        &6u32,
        &Symbol::new(&t.env, "real_estate"),
        &Map::new(&t.env),
        &t.compliance_registry.address.clone(),
        &t.dividend_distributor.address.clone(),
    );

    // Set up KYC for users
    let kyc_status = KYCStatus {
        is_verified: true,
        verification_level: 2,
        expiry_date: t.env.ledger().timestamp() + 86400 * 365,
        jurisdiction: Symbol::new(&t.env, "US"),
        is_accredited: true,
        risk_score: 3,
        aml_flags: Vec::new(&t.env),
    };

    t.compliance_registry
        .update_kyc_status(&t.admin, &t.user1, &kyc_status.clone());
    t.compliance_registry
        .update_kyc_status(&t.admin, &t.user2, &kyc_status);

    // Transfer tokens to users
    t.rwa_token.transfer(&t.admin, &t.user1, &100_000i128);
    t.rwa_token.transfer(&t.admin, &t.user2, &200_000i128);

    // Create multi-currency distributions
    let currencies = Vec::from_array(&t.env, [Symbol::new(&t.env, "USDC")]);
    let amounts = Vec::from_array(&t.env, [10_000i128]);

    let distribution_ids = t.dividend_distributor.multi_ccy_distributions(
        &t.admin,
        &t.rwa_token.address.clone(),
        &currencies,
        &amounts,
        &(t.env.ledger().timestamp() + 86400 * 30),
        &Map::new(&t.env),
    );

    assert_eq!(distribution_ids.len(), 1);

    // Both users claim dividends
    let claimed1 = t
        .dividend_distributor
        .claim_dividend(&distribution_ids.get(0).unwrap(), &t.user1.clone());
    let claimed2 = t
        .dividend_distributor
        .claim_dividend(&distribution_ids.get(0).unwrap(), &t.user2.clone());

    // user2 should claim more (has 2x tokens)
    assert!(claimed2 > claimed1);
}

// ── Integration Test 7: Emergency Pause and Recovery ────────────────────────────

#[test]
fn test_emergency_pause_and_recovery() {
    let t = setup_integration_test();

    // Initialize RWA Token
    t.rwa_token.initialize(
        &t.admin,
        &Symbol::new(&t.env, "PauseToken"),
        &Symbol::new(&t.env, "PAUS"),
        &1_000_000i128,
        &6u32,
        &Symbol::new(&t.env, "security"),
        &Map::new(&t.env),
        &t.compliance_registry.address.clone(),
        &t.dividend_distributor.address.clone(),
    );

    // Transfer tokens to user1
    t.rwa_token.transfer(&t.admin, &t.user1, &100_000i128);

    // Pause token
    t.rwa_token.pause(&t.admin);

    // Transfer should fail while paused
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        t.rwa_token.transfer(&t.user1, &t.user2, &1000i128);
    }));
    assert!(result.is_err());

    // Unpause token
    t.rwa_token.unpause(&t.admin);

    // Transfer should now succeed
    t.rwa_token.transfer(&t.user1, &t.user2, &1000i128);

    let user2_balance = t.rwa_token.get_balance(&t.user2);
    assert_eq!(user2_balance.amount, 1000);
}

// ── Integration Test 8: Asset Factory Emergency Pause All ────────────────────

#[test]
fn test_asset_factory_emergency_pause_all() {
    let t = setup_integration_test();

    // Register and deploy multiple assets
    let wasm_hash = BytesN::from_array(&t.env, &[1u8; 32]);

    for i in 0..3 {
        let asset_config = AssetConfig {
            name: Symbol::new(&t.env, &format!("Asset{}", i)),
            symbol: Symbol::new(&t.env, &format!("AST{}", i)),
            decimals: 6,
            total_supply: 1_000_000i128,
            asset_class: AssetClass::RealEstate,
            compliance_rules: ComplianceRules {
                kyc_required: true,
                accredited_investor_only: false,
                geographic_restrictions: Vec::new(&t.env),
                holding_period_days: 0,
                transfer_limits: 1_000_000i128,
            },
            dividend_schedule: Vec::new(&t.env),
            metadata: Map::new(&t.env),
        };

        let template = AssetTemplate {
            asset_class: AssetClass::RealEstate,
            base_config: asset_config,
            wasm_hash: wasm_hash.clone(),
            is_active: true,
            version: 1,
        };

        t.asset_factory.register_template(&t.admin, &template);
    }

    // Emergency pause all assets
    t.asset_factory.emergency_pause_all(&t.admin);

    // Verify all assets are paused
    let assets = t.asset_factory.list_assets();
    // Note: Since we only registered templates but didn't create assets,
    // this tests the emergency pause function doesn't panic on empty registry
    assert_eq!(assets.len(), 0);
}

// ── Integration Test 9: Transfer Limits Enforcement ────────────────────────────

#[test]
fn test_transfer_limits_enforcement() {
    let t = setup_integration_test();

    // Initialize RWA Token
    t.rwa_token.initialize(
        &t.admin,
        &Symbol::new(&t.env, "LimitToken"),
        &Symbol::new(&t.env, "LIMT"),
        &1_000_000i128,
        &6u32,
        &Symbol::new(&t.env, "security"),
        &Map::new(&t.env),
        &t.compliance_registry.address.clone(),
        &t.dividend_distributor.address.clone(),
    );

    // Set up KYC for user1
    let kyc_status = KYCStatus {
        is_verified: true,
        verification_level: 2,
        expiry_date: t.env.ledger().timestamp() + 86400 * 365,
        jurisdiction: Symbol::new(&t.env, "US"),
        is_accredited: true,
        risk_score: 3,
        aml_flags: Vec::new(&t.env),
    };

    t.compliance_registry
        .update_kyc_status(&t.admin, &t.user1, &kyc_status);

    // Transfer tokens to user1
    t.rwa_token.transfer(&t.admin, &t.user1, &100_000i128);

    // Set transfer limits for user1 (daily limit: 5000)
    use crate::compliance_registry::TransferLimits;
    let limits = TransferLimits {
        daily_limit: 5000,
        monthly_limit: 50000,
        annual_limit: 500000,
        remaining_daily: 5000,
        remaining_monthly: 50000,
        remaining_annual: 500000,
        last_reset_daily: 0,
        last_reset_monthly: 0,
        last_reset_annual: 0,
    };

    t.compliance_registry
        .set_transfer_limits(&t.admin, &t.user1, &limits);

    // Transfer within limit should succeed
    t.rwa_token.transfer(&t.user1, &t.user2, &1000i128);

    let user2_balance = t.rwa_token.get_balance(&t.user2);
    assert_eq!(user2_balance.amount, 1000);

    // Transfer exceeding limit should fail
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        t.rwa_token.transfer(&t.user1, &t.user2, &5000i128);
    }));
    assert!(result.is_err());
}

// ── Integration Test 10: Full Workflow - End to End ────────────────────────────

#[test]
fn test_full_workflow_end_to_end() {
    let t = setup_integration_test();

    // Reset budget before workflow for gas tracking
    t.env.budget().reset_default();

    // Step 1: Initialize RWA Token
    t.rwa_token.initialize(
        &t.admin,
        &Symbol::new(&t.env, "FullWorkflowToken"),
        &Symbol::new(&t.env, "FWFT"),
        &1_000_000i128,
        &6u32,
        &Symbol::new(&t.env, "real_estate"),
        &Map::new(&t.env),
        &t.compliance_registry.address.clone(),
        &t.dividend_distributor.address.clone(),
    );

    // Step 2: Set up KYC for users
    let kyc_status = KYCStatus {
        is_verified: true,
        verification_level: 2,
        expiry_date: t.env.ledger().timestamp() + 86400 * 365,
        jurisdiction: Symbol::new(&t.env, "US"),
        is_accredited: true,
        risk_score: 3,
        aml_flags: Vec::new(&t.env),
    };

    t.compliance_registry
        .update_kyc_status(&t.admin, &t.user1, &kyc_status.clone());
    t.compliance_registry
        .update_kyc_status(&t.admin, &t.user2, &kyc_status);

    // Step 3: Distribute tokens to users
    t.rwa_token.transfer(&t.admin, &t.user1, &200_000i128);
    t.rwa_token.transfer(&t.admin, &t.user2, &300_000i128);

    // Step 4: Lock tokens for governance
    t.rwa_token
        .lock_tokens(&t.user1, &t.user1, &50_000i128, &3600u64);
    t.rwa_token
        .lock_tokens(&t.user2, &t.user2, &100_000i128, &3600u64);

    // Step 5: Create dividend distribution
    let distribution_id = t.dividend_distributor.create_distribution(
        &t.admin,
        &t.rwa_token.address.clone(),
        &Symbol::new(&t.env, "USDC"),
        &50_000i128,
        &(t.env.ledger().timestamp() + 86400 * 30),
        &Map::new(&t.env),
    );

    // Step 6: Users claim dividends
    let claimed1 = t
        .dividend_distributor
        .claim_dividend(&distribution_id, &t.user1.clone());
    let claimed2 = t
        .dividend_distributor
        .claim_dividend(&distribution_id, &t.user2.clone());

    assert!(claimed1 > 0);
    assert!(claimed2 > 0);

    // Step 7: Place sell order on secondary market
    let sell_order_id = t.secondary_market.place_order(
        &t.user1,
        &t.rwa_token.address.clone(),
        &Symbol::new(&t.env, "sell"),
        &150i128,
        &10_000i128,
        &(t.env.ledger().timestamp() + 86400),
        &1000i128,
    );

    // Step 8: Fill the order
    t.secondary_market
        .fill_order(&t.user2, &sell_order_id, &10_000i128);

    // Step 9: Verify final state
    let user1_balance = t.rwa_token.get_balance(&t.user1);
    let user2_balance = t.rwa_token.get_balance(&t.user2);

    // user1: 200,000 - 50,000 locked - 10,000 sold = 140,000
    assert_eq!(user1_balance.amount, 140_000);
    assert_eq!(user1_balance.locked_amount, 50_000);

    // user2: 300,000 - 100,000 locked + 10,000 bought = 210,000
    assert_eq!(user2_balance.amount, 210_000);
    assert_eq!(user2_balance.locked_amount, 100_000);

    // Step 10: Verify dividend claims
    let claim_info1 = t
        .dividend_distributor
        .get_claim_info(&distribution_id, &t.user1.clone());
    let claim_info2 = t
        .dividend_distributor
        .get_claim_info(&distribution_id, &t.user2.clone());

    assert!(claim_info1.is_some());
    assert!(claim_info2.is_some());

    // Gas tracking: verify the full workflow stays within budget
    let gas_used = t.env.budget().cpu_instruction_cost();
    let mem_bytes = t.env.budget().memory_bytes_cost();

    // Full workflow should complete well within Soroban transaction limits
    // (max ~100M CPU instructions, ~40MB memory per transaction)
    assert!(gas_used > 0, "Gas should be consumed during full workflow");
    assert!(
        gas_used < 100_000_000,
        "Full workflow should stay within CPU budget"
    );
    assert!(
        mem_bytes < 40_000_000,
        "Full workflow should stay within memory budget"
    );

    eprintln!(
        "Full workflow gas used: {} CPU insns, {} mem bytes",
        gas_used, mem_bytes
    );

    // Step 11: Verify custody attestation
    let attestation = CustodyAttestation {
        asset_id: t.rwa_token.address.clone(),
        custodian: t.admin.clone(),
        location: Symbol::new(&t.env, "NYC Vault"),
        condition: Symbol::new(&t.env, "excellent"),
        value: 1_000_000i128,
        timestamp: t.env.ledger().timestamp(),
        proof_hash: BytesN::from_array(&t.env, &[2u8; 32]),
        verification_type: Symbol::new(&t.env, "real_estate"),
        insurance_status: Symbol::new(&t.env, "insured"),
        legal_title_hash: BytesN::from_array(&t.env, &[3u8; 32]),
        audit_report_hash: BytesN::from_array(&t.env, &[4u8; 32]),
        multi_sig_signatures: Vec::from_array(
            &t.env,
            [
                BytesN::from_array(&t.env, &[5u8; 64]),
                BytesN::from_array(&t.env, &[6u8; 64]),
                BytesN::from_array(&t.env, &[7u8; 64]),
            ],
        ),
        metadata: Map::new(&t.env),
        is_valid: false,
        expires_at: t.env.ledger().timestamp() + 86400 * 60,
    };

    let attestation_id = t.custody_validator.submit_attestation(&attestation);
    assert_eq!(attestation_id, 1);

    let retrieved = t.custody_validator.get_attestation(&attestation_id);
    assert!(retrieved.is_valid);
    assert_eq!(retrieved.asset_id, t.rwa_token.address);
    assert_eq!(retrieved.value, 1_000_000);
}

// ── Integration Test 11: Unauthorized Access Rejection ─────────────────────────

#[test]
fn test_unauthorized_access_rejection() {
    let t = setup_integration_test();

    // Initialize RWA Token
    t.rwa_token.initialize(
        &t.admin,
        &Symbol::new(&t.env, "UnauthToken"),
        &Symbol::new(&t.env, "UNAT"),
        &1_000_000i128,
        &6u32,
        &Symbol::new(&t.env, "security"),
        &Map::new(&t.env),
        &t.compliance_registry.address.clone(),
        &t.dividend_distributor.address.clone(),
    );

    // Attempt to pause from non-admin should fail
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        t.rwa_token.pause(&t.user1);
    }));
    assert!(result.is_err(), "Non-admin should not be able to pause");

    // Attempt to mint from non-admin should fail
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        t.rwa_token.mint(&t.user1, &t.user1, &1000i128);
    }));
    assert!(result.is_err(), "Non-admin should not be able to mint");

    // Attempt to create distribution from non-admin should fail
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        t.dividend_distributor.create_distribution(
            &t.user1,
            &t.rwa_token.address.clone(),
            &Symbol::new(&t.env, "USDC"),
            &10_000i128,
            &(t.env.ledger().timestamp() + 86400),
            &Map::new(&t.env),
        );
    }));
    assert!(
        result.is_err(),
        "Non-admin should not be able to create distributions"
    );
}

// ── Integration Test 12: Deploy → KYC → Dividend → Custody Workflow ───────────

#[test]
fn test_deploy_kyc_dividend_custody_workflow() {
    let t = setup_integration_test();

    // Step 1: Deploy/Initialize RWA Token
    t.rwa_token.initialize(
        &t.admin,
        &Symbol::new(&t.env, "CustodyAsset"),
        &Symbol::new(&t.env, "CUST"),
        &1_000_000i128,
        &6u32,
        &Symbol::new(&t.env, "commodity"),
        &Map::new(&t.env),
        &t.compliance_registry.address.clone(),
        &t.dividend_distributor.address.clone(),
    );

    // Step 2: KYC for user1
    let kyc_status = KYCStatus {
        is_verified: true,
        verification_level: 2,
        expiry_date: t.env.ledger().timestamp() + 86400 * 365,
        jurisdiction: Symbol::new(&t.env, "US"),
        is_accredited: true,
        risk_score: 3,
        aml_flags: Vec::new(&t.env),
    };
    t.compliance_registry
        .update_kyc_status(&t.admin, &t.user1, &kyc_status);

    // Step 3: Transfer tokens
    t.rwa_token.transfer(&t.admin, &t.user1, &100_000i128);

    // Step 4: Create dividend distribution
    let distribution_id = t.dividend_distributor.create_distribution(
        &t.admin,
        &t.rwa_token.address.clone(),
        &Symbol::new(&t.env, "USDC"),
        &10_000i128,
        &(t.env.ledger().timestamp() + 86400 * 30),
        &Map::new(&t.env),
    );

    // Step 5: Claim dividend
    let claimed = t
        .dividend_distributor
        .claim_dividend(&distribution_id, &t.user1.clone());
    assert!(claimed > 0);

    // Step 6: Verify custody via attestation
    let attestation = CustodyAttestation {
        asset_id: t.rwa_token.address.clone(),
        custodian: t.admin.clone(),
        location: Symbol::new(&t.env, "Vault London"),
        condition: Symbol::new(&t.env, "good"),
        value: 500_000i128,
        timestamp: t.env.ledger().timestamp(),
        proof_hash: BytesN::from_array(&t.env, &[8u8; 32]),
        verification_type: Symbol::new(&t.env, "commodities"),
        insurance_status: Symbol::new(&t.env, "insured"),
        legal_title_hash: BytesN::from_array(&t.env, &[9u8; 32]),
        audit_report_hash: BytesN::from_array(&t.env, &[10u8; 32]),
        multi_sig_signatures: Vec::from_array(&t.env, [BytesN::from_array(&t.env, &[11u8; 64])]),
        metadata: Map::new(&t.env),
        is_valid: false,
        expires_at: t.env.ledger().timestamp() + 86400 * 90,
    };

    let attestation_id = t.custody_validator.submit_attestation(&attestation);
    assert_eq!(attestation_id, 1);

    let retrieved = t.custody_validator.get_attestation(&attestation_id);
    assert!(retrieved.is_valid);

    // Step 7: Verify final state - token balances intact
    let user1_balance = t.rwa_token.get_balance(&t.user1);
    assert_eq!(user1_balance.amount, 100_000);

    let admin_balance = t.rwa_token.get_balance(&t.admin);
    assert_eq!(admin_balance.amount, 900_000);
}

// ── Integration Test 13: Custody Attestation Expiry & Invalidation ─────────────

#[test]
fn test_custody_attestation_expiry_and_invalidation() {
    let t = setup_integration_test();

    let attestation = CustodyAttestation {
        asset_id: t.rwa_token.address.clone(),
        custodian: t.admin.clone(),
        location: Symbol::new(&t.env, "Vault London"),
        condition: Symbol::new(&t.env, "good"),
        value: 500_000i128,
        timestamp: t.env.ledger().timestamp(),
        proof_hash: BytesN::from_array(&t.env, &[8u8; 32]),
        verification_type: Symbol::new(&t.env, "commodities"),
        insurance_status: Symbol::new(&t.env, "insured"),
        legal_title_hash: BytesN::from_array(&t.env, &[9u8; 32]),
        audit_report_hash: BytesN::from_array(&t.env, &[10u8; 32]),
        multi_sig_signatures: Vec::from_array(&t.env, [BytesN::from_array(&t.env, &[11u8; 64])]),
        metadata: Map::new(&t.env),
        is_valid: false,
        expires_at: t.env.ledger().timestamp() + 100,
    };

    let attestation_id = t.custody_validator.submit_attestation(&attestation);
    assert!(t.custody_validator.is_attestation_valid(&attestation_id));

    // Admin invalidates attestation
    t.custody_validator
        .invalidate_attestation(&t.admin, &attestation_id);
    assert!(!t.custody_validator.is_attestation_valid(&attestation_id));
    let retrieved = t.custody_validator.get_attestation(&attestation_id);
    assert!(!retrieved.is_valid);
}

#[test]
fn test_custody_attestation_auto_expiry_check() {
    let t = setup_integration_test();

    let attestation = CustodyAttestation {
        asset_id: t.rwa_token.address.clone(),
        custodian: t.admin.clone(),
        location: Symbol::new(&t.env, "Vault NY"),
        condition: Symbol::new(&t.env, "good"),
        value: 500_000i128,
        timestamp: t.env.ledger().timestamp(),
        proof_hash: BytesN::from_array(&t.env, &[8u8; 32]),
        verification_type: Symbol::new(&t.env, "commodities"),
        insurance_status: Symbol::new(&t.env, "insured"),
        legal_title_hash: BytesN::from_array(&t.env, &[9u8; 32]),
        audit_report_hash: BytesN::from_array(&t.env, &[10u8; 32]),
        multi_sig_signatures: Vec::from_array(&t.env, [BytesN::from_array(&t.env, &[11u8; 64])]),
        metadata: Map::new(&t.env),
        is_valid: false,
        expires_at: t.env.ledger().timestamp() + 100,
    };

    let attestation_id = t.custody_validator.submit_attestation(&attestation);

    // Fast forward ledger timestamp past expiry
    t.env
        .ledger()
        .set_timestamp(t.env.ledger().timestamp() + 200);

    // get_attestation checks expiry and updates stored is_valid to false
    let retrieved = t.custody_validator.get_attestation(&attestation_id);
    assert!(!retrieved.is_valid);
    assert!(!t.custody_validator.is_attestation_valid(&attestation_id));
}

// ── Integration Test 14: Oracle Reputation Decay ───────────────────────────────

#[test]
fn test_oracle_reputation_decay() {
    let t = setup_integration_test();

    let oracle_info = t.custody_validator.get_oracle_info(&t.oracle1);
    let initial_score = oracle_info.reputation_score;

    // Fast forward 14 days (2 decay periods of 7 days each)
    t.env
        .ledger()
        .set_timestamp(t.env.ledger().timestamp() + 14 * 86400);

    let decayed_info = t.custody_validator.get_oracle_info(&t.oracle1);
    assert_eq!(decayed_info.reputation_score, initial_score - 2);

    // Reset reputation
    t.custody_validator
        .update_oracle_reputation(&t.admin, &t.oracle1, &90u32);
    let updated_info = t.custody_validator.get_oracle_info(&t.oracle1);
    assert_eq!(updated_info.reputation_score, 90);
}

// ── Integration Test 15: Governance Proposal Categorization & Timelocks ────────

#[test]
fn test_governance_proposal_categorization_and_timelock() {
    let env = Env::default();
    env.mock_all_auths();

    let owner1 = Address::generate(&env);
    let owner2 = Address::generate(&env);
    let owners = Vec::from_array(&env, [owner1.clone(), owner2.clone()]);

    crate::shared_governance::write_governance(&env, &owner1, &owners, 2, 3600);

    // Create a parameter-change proposal (14 day review period + 3600s timelock)
    let payload = BytesN::from_array(&env, &[1u8; 32]);
    let prop_key = Symbol::new(&env, "upgrade_v2");

    let pid = crate::shared_governance::create_proposal_with_model(
        env.clone(),
        owner1.clone(),
        prop_key.clone(),
        crate::shared_governance::ProposalCategory::ParameterChange,
        payload.clone(),
        crate::shared_governance::VotingModel::Linear,
    );
    assert_eq!(pid, 1);

    crate::shared_governance::approve(env.clone(), owner1.clone(), pid);
    crate::shared_governance::approve(env.clone(), owner2.clone(), pid);

    // Cannot execute immediately because review period & timelock haven't expired
    assert!(!crate::shared_governance::can_execute(
        &env,
        pid,
        prop_key.clone(),
        payload.clone()
    ));

    // Fast forward past review period (14 days = 1_209_600s) + 3600s timelock
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + 1_209_600 + 3601);

    assert!(crate::shared_governance::can_execute(
        &env,
        pid,
        prop_key.clone(),
        payload
    ));
    crate::shared_governance::execute_mark(env.clone(), owner1, pid);
}
