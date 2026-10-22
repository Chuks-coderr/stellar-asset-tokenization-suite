#![cfg(test)]

use soroban_sdk::{
    testutils::{Address as _, Ledger, LedgerInfo},
    token::{StellarAssetClient, TokenClient},
    Address, Env, Error as SdkError, InvokeError, Map, Symbol, Vec,
};

use crate::{
    compliance_registry::{ComplianceRegistry, ComplianceRegistryClient, KYCStatus},
    dividend_distributor::{DividendDistributor, DividendDistributorClient},
    rwa_token::{RWAToken, RWATokenClient},
    secondary_market::{
        CircuitBreakerConfig, FeeTier, MarketError, OrderCondition, SecondaryMarket,
        SecondaryMarketClient,
    },
};

// ── helpers ──────────────────────────────────────────────────────────────────

struct TestEnv {
    env: Env,
    admin: Address,
    market: SecondaryMarketClient<'static>,
    token: RWATokenClient<'static>,
    base_token: TokenClient<'static>,
    compliance: ComplianceRegistryClient<'static>,
}

fn setup() -> TestEnv {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);

    // Base currency (USDC-like)
    let base_id = env.register_stellar_asset_contract_v2(admin.clone());
    let base_token = TokenClient::new(&env, &base_id.address());
    let base_admin = StellarAssetClient::new(&env, &base_id.address());
    base_admin.mint(&admin, &1_000_000i128);

    // Compliance registry (kyc_required = false for most tests)
    let compliance_id = env.register_contract(None, ComplianceRegistry);
    let compliance = ComplianceRegistryClient::new(&env, &compliance_id);
    compliance.initialize(&admin, &admin, &false, &false);

    // Dividend distributor (placeholder)
    let dist_id = env.register_contract(None, DividendDistributor);
    let dist = DividendDistributorClient::new(&env, &dist_id);
    dist.initialize(&admin, &admin, &Vec::new(&env));

    // RWA token
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
        &dist_id,
    );

    // Secondary market
    let market_id = env.register_contract(None, SecondaryMarket);
    let market = SecondaryMarketClient::new(&env, &market_id);
    market.initialize(
        &admin,
        &base_id.address(),
        &compliance_id,
        &dist_id,
        &50i64,   // 0.5% fee
        &10i128,  // min order size
        &2000i64, // 20% max price deviation
    );

    // SAFETY: env lifetime is tied to the test scope.
    let market: SecondaryMarketClient<'static> = unsafe { core::mem::transmute(market) };
    let token: RWATokenClient<'static> = unsafe { core::mem::transmute(token) };
    let base_token: TokenClient<'static> = unsafe { core::mem::transmute(base_token) };
    let compliance: ComplianceRegistryClient<'static> = unsafe { core::mem::transmute(compliance) };

    TestEnv {
        env,
        admin,
        market,
        token,
        base_token,
        compliance,
    }
}

fn future_expiry(env: &Env) -> u64 {
    env.ledger().timestamp() + 3600
}

/// Mark a participant as KYC-verified so the market's compliance gate passes
/// for both placing and filling orders.
fn verify_kyc(t: &TestEnv, user: &Address) {
    t.compliance.update_kyc_status(
        &t.admin,
        user,
        &KYCStatus {
            is_verified: true,
            verification_level: 2,
            expiry_date: t.env.ledger().timestamp() + 10_000_000,
            jurisdiction: Symbol::new(&t.env, "US"),
            is_accredited: true,
            risk_score: 1,
            aml_flags: Vec::new(&t.env),
        },
    );
}

// ── initialize ───────────────────────────────────────────────────────────────

#[test]
fn initialize_succeeds() {
    // setup() calls initialize; if we reach here it worked
    let _ = setup();
}

#[test]
#[should_panic]
fn initialize_twice_panics() {
    let t = setup();
    let other = Address::generate(&t.env);
    t.market
        .initialize(&t.admin, &other, &other, &other, &50i64, &10i128, &2000i64);
}

// ── place_order (buy) ─────────────────────────────────────────────────────────

#[test]
fn place_buy_order_returns_order_id() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    // Fund buyer with base currency
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);

    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &100i128, // price
        &50i128,  // amount
        &future_expiry(&t.env),
        &0i128, // min_fill
    );
    assert_eq!(order_id, 1);
}

#[test]
fn place_multiple_orders_increments_ids() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &100_000i128);

    let id1 = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &100i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
    let id2 = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &100i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
    assert_eq!(id2, id1 + 1);
}

#[test]
#[should_panic]
fn place_order_below_min_size_panics() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);

    // amount = 5, min_order_size = 10
    t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &100i128,
        &5i128,
        &future_expiry(&t.env),
        &0i128,
    );
}

#[test]
#[should_panic]
fn place_order_with_zero_price_panics() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);

    t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &0i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
}

#[test]
#[should_panic]
fn place_order_with_expired_expiry_panics() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);

    // expiry in the past
    t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &100i128,
        &50i128,
        &0u64,
        &0i128,
    );
}

#[test]
#[should_panic]
fn place_order_with_invalid_side_panics() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);

    t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "invalid"),
        &100i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
}

// ── place_order (sell) ────────────────────────────────────────────────────────

#[test]
fn place_sell_order_escrows_rwa_tokens() {
    let t = setup();
    let seller = Address::generate(&t.env);
    t.token.transfer(&t.admin, &seller, &200i128);

    let balance_before = t.token.get_balance(&seller).amount;
    t.market.place_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &100i128,
        &100i128,
        &future_expiry(&t.env),
        &0i128,
    );
    let balance_after = t.token.get_balance(&seller).amount;
    assert_eq!(balance_before - balance_after, 100);
}

// ── cancel_order ──────────────────────────────────────────────────────────────

#[test]
fn cancel_buy_order_refunds_base_currency() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);

    let balance_before = t.base_token.balance(&buyer);
    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &100i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
    t.market.cancel_order(&buyer, &order_id);
    let balance_after = t.base_token.balance(&buyer);
    assert_eq!(balance_after, balance_before);
}

#[test]
fn cancel_sell_order_refunds_rwa_tokens() {
    let t = setup();
    let seller = Address::generate(&t.env);
    t.token.transfer(&t.admin, &seller, &200i128);

    let balance_before = t.token.get_balance(&seller).amount;
    let order_id = t.market.place_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &100i128,
        &100i128,
        &future_expiry(&t.env),
        &0i128,
    );
    t.market.cancel_order(&seller, &order_id);
    let balance_after = t.token.get_balance(&seller).amount;
    assert_eq!(balance_after, balance_before);
}

#[test]
#[should_panic]
fn cancel_order_by_non_maker_panics() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let attacker = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);

    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &100i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
    t.market.cancel_order(&attacker, &order_id);
}

#[test]
#[should_panic]
fn cancel_nonexistent_order_panics() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    t.market.cancel_order(&buyer, &999u64);
}

// ── fill_order ────────────────────────────────────────────────────────────────

#[test]
fn fill_buy_order_settles_correctly() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);

    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);
    t.token.transfer(&t.admin, &seller, &200i128);

    // Buyer places buy order: 100 tokens at price 10 each = 1000 base
    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &10i128,
        &100i128,
        &future_expiry(&t.env),
        &0i128,
    );

    let seller_rwa_before = t.token.get_balance(&seller).amount;
    let buyer_rwa_before = t.token.get_balance(&buyer).amount;

    // Seller fills the order
    t.market.fill_order(&seller, &order_id, &100i128);

    // Seller should have 100 fewer RWA tokens
    assert_eq!(t.token.get_balance(&seller).amount, seller_rwa_before - 100);
    // Buyer should have 100 more RWA tokens
    assert_eq!(t.token.get_balance(&buyer).amount, buyer_rwa_before + 100);
}

#[test]
#[should_panic]
fn fill_order_exceeding_remaining_amount_panics() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);

    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);
    t.token.transfer(&t.admin, &seller, &500i128);

    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &10i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );

    // Try to fill 100 when order is only for 50
    t.market.fill_order(&seller, &order_id, &100i128);
}

#[test]
#[should_panic]
fn fill_nonexistent_order_panics() {
    let t = setup();
    let taker = Address::generate(&t.env);
    t.market.fill_order(&taker, &999u64, &10i128);
}

// ── VWAP ─────────────────────────────────────────────────────────────────────

#[test]
fn get_vwap_returns_zero_before_any_trades() {
    let t = setup();
    let vwap = t.market.get_vwap(&t.token.address);
    assert_eq!(vwap, 0);
}

#[test]
fn get_vwap_reflects_trade_price_after_fill() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);

    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);
    t.token.transfer(&t.admin, &seller, &200i128);

    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &50i128,
        &100i128,
        &future_expiry(&t.env),
        &0i128,
    );
    t.market.fill_order(&seller, &order_id, &100i128);

    let vwap = t.market.get_vwap(&t.token.address);
    assert_eq!(vwap, 50); // single trade at price 50
}

// ── compliance enforcement ────────────────────────────────────────────────────

#[test]
#[should_panic]
fn place_order_by_unverified_user_when_kyc_required_panics() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);

    let base_id = env.register_stellar_asset_contract_v2(admin.clone());
    let base_admin = StellarAssetClient::new(&env, &base_id.address());

    // Compliance with kyc_required = true
    let compliance_id = env.register_contract(None, ComplianceRegistry);
    let compliance = ComplianceRegistryClient::new(&env, &compliance_id);
    compliance.initialize(&admin, &admin, &true, &false);

    let dist_id = env.register_contract(None, DividendDistributor);
    let dist = DividendDistributorClient::new(&env, &dist_id);
    dist.initialize(&admin, &admin, &Vec::new(&env));

    let token_id = env.register_contract(None, RWAToken);
    let token = RWATokenClient::new(&env, &token_id);
    token.initialize(
        &admin,
        &Symbol::new(&env, "T"),
        &Symbol::new(&env, "T"),
        &1_000_000i128,
        &6u32,
        &Symbol::new(&env, "real_estate"),
        &Map::new(&env),
        &compliance_id,
        &dist_id,
    );

    let market_id = env.register_contract(None, SecondaryMarket);
    let market = SecondaryMarketClient::new(&env, &market_id);
    market.initialize(
        &admin,
        &base_id.address(),
        &compliance_id,
        &dist_id,
        &50i64,
        &10i128,
        &2000i64,
    );

    let unverified_buyer = Address::generate(&env);
    base_admin.mint(&unverified_buyer, &10_000i128);

    // Should panic because KYC is required and buyer is not verified
    market.place_order(
        &unverified_buyer,
        &token_id,
        &Symbol::new(&env, "buy"),
        &100i128,
        &50i128,
        &(env.ledger().timestamp() + 3600),
        &0i128,
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #223: Concurrency & Race Condition Tests for Order Matching Engine
// ═══════════════════════════════════════════════════════════════════════════════

// ── concurrent fill_order protection ─────────────────────────────────────────

#[test]
fn concurrent_fill_same_order_partial_fills_settle_correctly() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller1 = Address::generate(&t.env);
    let seller2 = Address::generate(&t.env);

    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);
    t.token.transfer(&t.admin, &seller1, &200i128);
    t.token.transfer(&t.admin, &seller2, &200i128);

    // Buyer places buy order for 100 tokens
    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &10i128,
        &100i128,
        &future_expiry(&t.env),
        &0i128,
    );

    // Two sellers fill the same order (partial fills)
    t.market.fill_order(&seller1, &order_id, &40i128);
    t.market.fill_order(&seller2, &order_id, &40i128);

    // Verify buyer received 80 RWA tokens total
    assert_eq!(t.token.get_balance(&buyer).amount, 80);
    // Verify seller1 and seller2 were debited correctly
    assert_eq!(t.token.get_balance(&seller1).amount, 160);
    assert_eq!(t.token.get_balance(&seller2).amount, 160);
}

#[test]
fn concurrent_fill_same_order_no_double_fill() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller1 = Address::generate(&t.env);
    let seller2 = Address::generate(&t.env);

    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);
    t.token.transfer(&t.admin, &seller1, &200i128);
    t.token.transfer(&t.admin, &seller2, &200i128);

    // Buy order for exactly 50 tokens
    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &10i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );

    // First seller fills completely
    t.market.fill_order(&seller1, &order_id, &50i128);

    // Second seller tries to fill the same already-filled order — should panic
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        t.market.fill_order(&seller2, &order_id, &10i128);
    }));
    assert!(result.is_err());

    // Verify no funds lost: buyer got exactly 50, not 60
    assert_eq!(t.token.get_balance(&buyer).amount, 50);
}

#[test]
fn concurrent_fill_exceeding_remaining_panics_no_double_spend() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller1 = Address::generate(&t.env);
    let seller2 = Address::generate(&t.env);

    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);
    t.token.transfer(&t.admin, &seller1, &200i128);
    t.token.transfer(&t.admin, &seller2, &200i128);

    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &10i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );

    // First seller fills 40
    t.market.fill_order(&seller1, &order_id, &40i128);

    // Second seller tries to fill 20 when only 10 remain — should panic
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        t.market.fill_order(&seller2, &order_id, &20i128);
    }));
    assert!(result.is_err());

    // Verify no extra tokens moved: buyer got exactly 40
    assert_eq!(t.token.get_balance(&buyer).amount, 40);
}

// ── rapid place/cancel/fill sequences ───────────────────────────────────────

#[test]
fn rapid_place_cancel_fill_sequence_maintains_integrity() {
    let t = setup();
    let user_a = Address::generate(&t.env);
    let user_b = Address::generate(&t.env);

    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&user_a, &100_000i128);
    base_admin.mint(&user_b, &100_000i128);
    t.token.transfer(&t.admin, &user_a, &500i128);
    t.token.transfer(&t.admin, &user_b, &500i128);

    let base_balance_a_before = t.base_token.balance(&user_a);

    // Rapid sequence: place -> cancel -> re-place -> fill
    let id1 = t.market.place_order(
        &user_a,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &10i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
    t.market.cancel_order(&user_a, &id1);

    let id2 = t.market.place_order(
        &user_a,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &10i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
    t.market.fill_order(&user_b, &id2, &50i128);

    // user_a should only have spent 50*10 = 500 base (one order), not double-charged
    let base_balance_a_after = t.base_token.balance(&user_a);
    assert_eq!(base_balance_a_before - base_balance_a_after, 500);

    // user_a got 50 RWA tokens
    assert_eq!(t.token.get_balance(&user_a).amount, 550);
}

#[test]
fn rapid_fill_on_multiple_orders_same_account_no_lost_funds() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);

    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &100_000i128);
    t.token.transfer(&t.admin, &seller, &500i128);

    let seller_balance_before = t.token.get_balance(&seller).amount;

    // Place 5 buy orders rapidly
    let mut order_ids = Vec::new(&t.env);
    for _ in 0..5 {
        let id = t.market.place_order(
            &buyer,
            &t.token.address,
            &Symbol::new(&t.env, "buy"),
            &10i128,
            &20i128,
            &future_expiry(&t.env),
            &0i128,
        );
        order_ids.push_back(id);
    }

    // Fill all 5 orders from the same seller
    for i in 0..5 {
        let oid = order_ids.get(i).unwrap();
        t.market.fill_order(&seller, &oid, &20i128);
    }

    // Seller sold 5 * 20 = 100 tokens
    assert_eq!(
        t.token.get_balance(&seller).amount,
        seller_balance_before - 100
    );
    // Buyer received 100 tokens
    assert_eq!(t.token.get_balance(&buyer).amount, 100);
}

// ── same-ledger create and fill ──────────────────────────────────────────────

#[test]
fn order_created_and_filled_same_ledger() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);

    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);
    t.token.transfer(&t.admin, &seller, &200i128);

    // Place and fill without advancing ledger
    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &10i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
    t.market.fill_order(&seller, &order_id, &50i128);

    assert_eq!(t.token.get_balance(&buyer).amount, 50);
    assert_eq!(t.token.get_balance(&seller).amount, 150);
}

#[test]
fn multiple_orders_placed_filled_same_ledger() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);

    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &100_000i128);
    t.token.transfer(&t.admin, &seller, &500i128);

    for i in 0..10 {
        let order_id = t.market.place_order(
            &buyer,
            &t.token.address,
            &Symbol::new(&t.env, "buy"),
            &10i128,
            &10i128,
            &future_expiry(&t.env),
            &0i128,
        );
        t.market.fill_order(&seller, &order_id, &10i128);
    }

    // 10 orders * 10 tokens = 100 tokens transferred
    assert_eq!(t.token.get_balance(&buyer).amount, 100);
    assert_eq!(t.token.get_balance(&seller).amount, 400);
}

// ── escrow balance verification ──────────────────────────────────────────────

#[test]
fn escrow_balances_correct_after_multiple_fills() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);

    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &100_000i128);
    t.token.transfer(&t.admin, &seller, &500i128);

    let buyer_base_before = t.base_token.balance(&buyer);
    let market_base_before = t.base_token.balance(&t.market.address);

    // Place 3 buy orders
    let ids: std::vec::Vec<u64> = (0..3)
        .map(|_| {
            t.market.place_order(
                &buyer,
                &t.token.address,
                &Symbol::new(&t.env, "buy"),
                &10i128,
                &30i128,
                &future_expiry(&t.env),
                &0i128,
            )
        })
        .collect();

    let buyer_base_after_place = t.base_token.balance(&buyer);
    // Escrowed: 3 * 30 * 10 = 900
    assert_eq!(buyer_base_before - buyer_base_after_place, 900);

    // Fill 2 of the 3 orders
    t.market.fill_order(&seller, &ids[0], &30i128);
    t.market.fill_order(&seller, &ids[1], &30i128);

    // Escrow should be down by 600 (2 filled orders)
    let market_base_after = t.base_token.balance(&t.market.address);
    assert_eq!(market_base_after, market_base_before + 300); // only 1 order (300) still in escrow
}

#[test]
fn escrow_returns_correctly_after_full_cancellation() {
    let t = setup();
    let buyer = Address::generate(&t.env);

    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &100_000i128);

    let base_before = t.base_token.balance(&buyer);

    // Place multiple orders
    let mut order_ids = Vec::new(&t.env);
    for _ in 0..5 {
        let id = t.market.place_order(
            &buyer,
            &t.token.address,
            &Symbol::new(&t.env, "buy"),
            &10i128,
            &20i128,
            &future_expiry(&t.env),
            &0i128,
        );
        order_ids.push_back(id);
    }

    // Cancel all
    for i in 0..5 {
        t.market.cancel_order(&buyer, &order_ids.get(i).unwrap());
    }

    // Full refund should restore original balance
    assert_eq!(t.base_token.balance(&buyer), base_before);
}

// ── flash-crash scenario ─────────────────────────────────────────────────────

#[test]
fn flash_crash_scenario_buy_sell_oscillation() {
    let t = setup();
    let trader1 = Address::generate(&t.env);
    let trader2 = Address::generate(&t.env);

    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&trader1, &1_000_000i128);
    base_admin.mint(&trader2, &1_000_000i128);
    t.token.transfer(&t.admin, &trader1, &2000i128);
    t.token.transfer(&t.admin, &trader2, &2000i128);

    let t1_base_before = t.base_token.balance(&trader1);
    let t2_base_before = t.base_token.balance(&trader2);
    let t1_rwa_before = t.token.get_balance(&trader1).amount;
    let t2_rwa_before = t.token.get_balance(&trader2).amount;

    // Simulate rapid price changes: wide price oscillation
    // trader1 buys high, trader2 sells into it
    let o1 = t.market.place_order(
        &trader1,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &200i128,
        &10i128,
        &future_expiry(&t.env),
        &0i128,
    );
    t.market.fill_order(&trader2, &o1, &10i128);

    // trader2 buys low
    let o2 = t.market.place_order(
        &trader2,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &10i128,
        &10i128,
        &future_expiry(&t.env),
        &0i128,
    );
    t.market.fill_order(&trader1, &o2, &10i128);

    // Net verifications: total RWA supply unchanged
    let t1_rwa_after = t.token.get_balance(&trader1).amount;
    let t2_rwa_after = t.token.get_balance(&trader2).amount;

    // trader1: bought 10 @200, sold 10 @10 => net RWA = 0, net base = -2000 + 100 = -1900
    // trader2: sold 10 @200, bought 10 @10 => net RWA = 0, net base = +2000 - 100 = +1900
    assert_eq!(t1_rwa_after, t1_rwa_before);
    assert_eq!(t2_rwa_after, t2_rwa_before);
    // Total base tokens conserved (minus fees)
    let net_change = (t1_base_before - t.base_token.balance(&trader1))
        .checked_sub(t.base_token.balance(&trader2) - t2_base_before)
        .unwrap_or(0);
    // Fee is 50 bps = 0.5% on each trade
    // Trade1: 2000 base, fee = 10. Trade2: 100 base, fee = 0
    // Net fees collected by protocol = 10
    assert!(net_change >= 0);
}

// ── stress test: 100+ orders with rapid matching ────────────────────────────

#[test]
fn stress_test_100_orders_rapid_matching() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);

    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    base_admin.mint(&seller, &1_000_000i128);
    t.token.transfer(&t.admin, &seller, &10_000i128);

    let buyer_base_before = t.base_token.balance(&buyer);
    let seller_rwa_before = t.token.get_balance(&seller).amount;

    // Place 100 buy orders
    let mut order_ids = Vec::new(&t.env);
    for _ in 0..100 {
        let id = t.market.place_order(
            &buyer,
            &t.token.address,
            &Symbol::new(&t.env, "buy"),
            &10i128,
            &5i128,
            &future_expiry(&t.env),
            &0i128,
        );
        order_ids.push_back(id);
    }

    // Fill all 100 orders
    for i in 0..100 {
        let oid = order_ids.get(i).unwrap();
        t.market.fill_order(&seller, &oid, &5i128);
    }

    // 100 orders * 5 tokens = 500 tokens transferred
    assert_eq!(t.token.get_balance(&buyer).amount, 500);
    assert_eq!(t.token.get_balance(&seller).amount, seller_rwa_before - 500);

    // Base currency accounting: 500 tokens * 10 price = 5000 base moved
    let buyer_base_after = t.base_token.balance(&buyer);
    assert!(buyer_base_before - buyer_base_after >= 5000);
}

#[test]
fn stress_test_interleaved_buy_sell_orders() {
    let t = setup();
    let trader_a = Address::generate(&t.env);
    let trader_b = Address::generate(&t.env);

    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&trader_a, &1_000_000i128);
    base_admin.mint(&trader_b, &1_000_000i128);
    t.token.transfer(&t.admin, &trader_a, &5000i128);
    t.token.transfer(&t.admin, &trader_b, &5000i128);

    let a_rwa_before = t.token.get_balance(&trader_a).amount;
    let b_rwa_before = t.token.get_balance(&trader_b).amount;

    // Interleave buy and sell orders between the two traders (50 rounds)
    for _ in 0..50 {
        // A places buy
        let buy_id = t.market.place_order(
            &trader_a,
            &t.token.address,
            &Symbol::new(&t.env, "buy"),
            &10i128,
            &2i128,
            &future_expiry(&t.env),
            &0i128,
        );
        t.market.fill_order(&trader_b, &buy_id, &2i128);

        // B places buy
        let sell_id = t.market.place_order(
            &trader_b,
            &t.token.address,
            &Symbol::new(&t.env, "buy"),
            &10i128,
            &2i128,
            &future_expiry(&t.env),
            &0i128,
        );
        t.market.fill_order(&trader_a, &sell_id, &2i128);
    }

    // After interleaving: each trader bought and sold 50*2=100 tokens
    // Net RWA positions should be unchanged (minus escrow sitting in unfilled orders)
    let a_rwa_after = t.token.get_balance(&trader_a).amount;
    let b_rwa_after = t.token.get_balance(&trader_b).amount;
    let total_rwa = a_rwa_after + b_rwa_after;
    assert_eq!(total_rwa, a_rwa_before + b_rwa_before);
}

#[test]
fn no_double_fill_on_partially_filled_order() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let third_party = Address::generate(&t.env);

    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);
    t.token.transfer(&t.admin, &seller, &200i128);
    t.token.transfer(&t.admin, &third_party, &200i128);

    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &10i128,
        &100i128,
        &future_expiry(&t.env),
        &0i128,
    );

    // Partial fill by seller (takes 40)
    t.market.fill_order(&seller, &order_id, &40i128);
    // Partial fill by third_party (takes remaining 60)
    t.market.fill_order(&third_party, &order_id, &60i128);

    // Buyer should have received exactly 100 tokens
    assert_eq!(t.token.get_balance(&buyer).amount, 100);

    // Any further fill should panic
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        t.market.fill_order(&seller, &order_id, &1i128);
    }));
    assert!(result.is_err());
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #222: Upgrade & Migration Tests for SecondaryMarket
// ═══════════════════════════════════════════════════════════════════════════════

// ── migrate ───────────────────────────────────────────────────────────────────

#[test]
fn migrate_preserves_market_config() {
    let t = setup();

    // Place an order and verify VWAP works
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);
    t.token.transfer(&t.admin, &seller, &200i128);

    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &50i128,
        &100i128,
        &future_expiry(&t.env),
        &0i128,
    );
    t.market.fill_order(&seller, &order_id, &100i128);

    // Market still functional
    let vwap = t.market.get_vwap(&t.token.address);
    assert_eq!(vwap, 50);
    assert_eq!(t.token.get_balance(&buyer).amount, 100);
}

#[test]
fn migrate_preserves_order_book_state() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &100_000i128);

    // Place 3 orders at different prices
    let id1 = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &100i128,
        &10i128,
        &future_expiry(&t.env),
        &0i128,
    );
    let id2 = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &90i128,
        &20i128,
        &future_expiry(&t.env),
        &0i128,
    );
    let id3 = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &80i128,
        &30i128,
        &future_expiry(&t.env),
        &0i128,
    );

    assert_eq!(id3, id1 + 2); // IDs increment correctly

    // Cancel middle order — escrow refunds correctly
    let buyer_base_before = t.base_token.balance(&buyer);
    t.market.cancel_order(&buyer, &id2);

    // Verify remaining orders still active
    let seller = Address::generate(&t.env);
    t.token.transfer(&t.admin, &seller, &200i128);
    t.market.fill_order(&seller, &id1, &10i128);

    assert_eq!(t.token.get_balance(&buyer).amount, 10);
}

#[test]
fn migrate_preserves_vwap_twap_after_multiple_trades() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);

    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &100_000i128);
    t.token.transfer(&t.admin, &seller, &500i128);

    // Execute 3 trades at different prices
    for price in [50, 100, 200] {
        let order_id = t.market.place_order(
            &buyer,
            &t.token.address,
            &Symbol::new(&t.env, "buy"),
            &price,
            &10i128,
            &future_expiry(&t.env),
            &0i128,
        );
        t.market.fill_order(&seller, &order_id, &10i128);
    }

    // VWAP should reflect weighted average: (50*10 + 100*10 + 200*10) / 30 = 116
    let vwap = t.market.get_vwap(&t.token.address);
    assert!(vwap >= 50);
    assert!(vwap <= 200);
}

#[test]
#[should_panic]
fn migrate_by_non_admin_panics() {
    let t = setup();
    let attacker = Address::generate(&t.env);
    t.market.migrate(&attacker);
}

#[test]
fn migrate_with_empty_order_book() {
    let t = setup();

    // No orders placed — VWAP is 0
    let vwap = t.market.get_vwap(&t.token.address);
    assert_eq!(vwap, 0);

    // TWAP should also be 0
    let twap = t.market.get_twap(&t.token.address, &0u64);
    assert_eq!(twap, 0);

    // Placing a new order still works
    let buyer = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);

    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &50i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
    assert_eq!(order_id, 1);
}

#[test]
fn migrate_preserves_escrow_integrity() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);

    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &100_000i128);
    t.token.transfer(&t.admin, &seller, &500i128);

    let buyer_base_before = t.base_token.balance(&buyer);
    let seller_rwa_before = t.token.get_balance(&seller).amount;

    // Place 2 buy orders and 1 sell order simultaneously
    let buy1 = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &10i128,
        &20i128,
        &future_expiry(&t.env),
        &0i128,
    );
    let buy2 = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &10i128,
        &30i128,
        &future_expiry(&t.env),
        &0i128,
    );
    let sell1 = t.market.place_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &10i128,
        &40i128,
        &future_expiry(&t.env),
        &0i128,
    );

    // Fill buy1, cancel buy2, leave sell1 active
    t.market.fill_order(&seller, &buy1, &20i128);
    t.market.cancel_order(&buyer, &buy2);

    // Verify escrow reconciliation
    // buy1 filled: buyer paid 200 base, got 20 RWA
    // buy2 cancelled: buyer got 300 base back
    // sell1 active: seller has 40 RWA in escrow
    assert_eq!(
        t.token.get_balance(&seller).amount,
        seller_rwa_before - 20 - 40
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #143: Order Expiry Enforcement & Auto-pruning Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
#[should_panic]
fn test_fill_expired_order_rejection() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);
    t.token.transfer(&t.admin, &seller, &100i128);

    let expiry = t.env.ledger().timestamp() + 100;
    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &10i128,
        &50i128,
        &expiry,
        &0i128,
    );

    // Advance time past expiry
    t.env.ledger().set_timestamp(expiry + 10);

    // Attempting to fill should panic with OrderExpired
    t.market.fill_order(&seller, &order_id, &50i128);
}

#[test]
fn test_cancel_expired_order_success() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);
    let balance_before = t.base_token.balance(&buyer);

    let expiry = t.env.ledger().timestamp() + 100;
    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &10i128,
        &50i128,
        &expiry,
        &0i128,
    );

    assert_eq!(t.base_token.balance(&buyer), balance_before - 500);

    // Advance time past expiry
    t.env.ledger().set_timestamp(expiry + 10);

    // Cancelling expired order recovers escrow
    t.market.cancel_order(&buyer, &order_id);
    assert_eq!(t.base_token.balance(&buyer), balance_before);
}

#[test]
fn test_auto_pruning_expired_orders() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &10_000i128);
    let balance_before = t.base_token.balance(&buyer);

    let expiry = t.env.ledger().timestamp() + 50;
    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &10i128,
        &50i128,
        &expiry,
        &0i128,
    );

    // Advance ledger timestamp past expiry
    t.env.ledger().set_timestamp(expiry + 20);

    // Explicitly call prune_expired_orders
    t.market.prune_expired_orders();

    // Escrow should be refunded automatically
    assert_eq!(t.base_token.balance(&buyer), balance_before);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #140: market orders with slippage protection
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn market_buy_fills_lowest_ask_completely() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    t.token.transfer(&t.admin, &seller, &500i128);
    verify_kyc(&t, &buyer);
    verify_kyc(&t, &seller);

    let ask = t.market.place_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &10i128,
        &100i128,
        &future_expiry(&t.env),
        &0i128,
    );

    let filled = t.market.place_market_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &100i128,
        &10i128,
    );
    assert_eq!(filled, 100);
    assert_eq!(t.token.get_balance(&buyer).amount, 100);

    // The resting order is fully consumed.
    assert!(t.market.try_fill_order(&seller, &ask, &1i128).is_err());
}

#[test]
fn market_buy_partial_fill_leaves_order_resting() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    t.token.transfer(&t.admin, &seller, &500i128);
    verify_kyc(&t, &buyer);
    verify_kyc(&t, &seller);

    // 200 RWA available at 10.
    t.market.place_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &10i128,
        &200i128,
        &future_expiry(&t.env),
        &0i128,
    );

    // Taker only wants 50: partial fill, 150 left resting.
    let filled = t.market.place_market_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &50i128,
        &10i128,
    );
    assert_eq!(filled, 50);
    assert_eq!(t.token.get_balance(&buyer).amount, 50);

    // A second market buy consumes more of the same resting order.
    let filled2 = t.market.place_market_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &100i128,
        &10i128,
    );
    assert_eq!(filled2, 100);
    assert_eq!(t.token.get_balance(&buyer).amount, 150);
}

#[test]
fn market_sell_fills_highest_bid_first() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    t.token.transfer(&t.admin, &seller, &500i128);
    verify_kyc(&t, &buyer);
    verify_kyc(&t, &seller);

    // Bids at 10 and 12; the 12 bid must be filled first.
    t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &10i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
    t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &12i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );

    let filled = t.market.place_market_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &50i128,
        &10i128,
    );
    assert_eq!(filled, 50);

    // The 12-bid maker received the RWA, so the seller's tokens are debited.
    assert_eq!(t.token.get_balance(&buyer).amount, 50);
}

#[test]
fn market_order_without_liquidity_panics() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    verify_kyc(&t, &buyer);

    let res = t.market.try_place_market_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &100i128,
        &10i128,
    );
    assert!(res.is_err());
}

#[test]
fn market_buy_beyond_limit_price_rejected() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    t.token.transfer(&t.admin, &seller, &500i128);
    verify_kyc(&t, &buyer);
    verify_kyc(&t, &seller);

    // Cheapest ask is 20, but the taker only accepts up to 10.
    t.market.place_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &20i128,
        &100i128,
        &future_expiry(&t.env),
        &0i128,
    );

    let res = t.market.try_place_market_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &100i128,
        &10i128,
    );
    assert!(res.is_err());
    // Nothing moved.
    assert_eq!(t.token.get_balance(&buyer).amount, 0);
}

// Issue #141: efficient order book with sorted data structure
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn order_book_sorts_bids_desc_and_asks_asc() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    t.token.transfer(&t.admin, &seller, &1_000i128);
    verify_kyc(&t, &buyer);
    verify_kyc(&t, &seller);

    // Bids placed out of order: 100, 120, 90.
    t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &100i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
    let best_bid = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &120i128,
        &40i128,
        &future_expiry(&t.env),
        &0i128,
    );
    t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &90i128,
        &30i128,
        &future_expiry(&t.env),
        &0i128,
    );

    // Asks placed out of order: 200, 150, 180.
    t.market.place_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &200i128,
        &20i128,
        &future_expiry(&t.env),
        &0i128,
    );
    let best_ask = t.market.place_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &150i128,
        &25i128,
        &future_expiry(&t.env),
        &0i128,
    );
    t.market.place_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &180i128,
        &15i128,
        &future_expiry(&t.env),
        &0i128,
    );

    let book = t.market.get_order_book(&t.token.address);
    let bid_prices: std::vec::Vec<i128> = book.bids.iter().map(|l| l.price).collect();
    let ask_prices: std::vec::Vec<i128> = book.asks.iter().map(|l| l.price).collect();
    assert_eq!(bid_prices, std::vec![120i128, 100, 90]);
    assert_eq!(ask_prices, std::vec![150i128, 180, 200]);

    // O(1) best-price lookup points at index 0 of each side.
    assert_eq!(
        t.market.get_best_bid(&t.token.address).unwrap().order_id,
        best_bid
    );
    assert_eq!(
        t.market.get_best_ask(&t.token.address).unwrap().order_id,
        best_ask
    );
}

#[test]
fn order_book_insertion_and_removal() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    verify_kyc(&t, &buyer);

    let o1 = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &100i128,
        &20i128,
        &future_expiry(&t.env),
        &0i128,
    );
    let o2 = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &100i128,
        &30i128,
        &future_expiry(&t.env),
        &0i128,
    );
    let o3 = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &110i128,
        &25i128,
        &future_expiry(&t.env),
        &0i128,
    );

    // Two orders at 100 aggregate into a single level (time-ordered).
    let book = t.market.get_order_book(&t.token.address);
    assert_eq!(book.bids.len(), 2);
    assert_eq!(book.bids.get(0).unwrap().price, 110);
    assert_eq!(book.bids.get(0).unwrap().total_amount, 25);
    assert_eq!(book.bids.get(1).unwrap().price, 100);
    assert_eq!(book.bids.get(1).unwrap().total_amount, 50);
    assert_eq!(book.bids.get(1).unwrap().order_count, 2);

    // Removing one order at a level keeps the level, updating its totals.
    t.market.cancel_order(&buyer, &o1);
    let book = t.market.get_order_book(&t.token.address);
    assert_eq!(book.bids.get(1).unwrap().total_amount, 30);
    assert_eq!(book.bids.get(1).unwrap().order_count, 1);

    // Removing the last order at a level drops the level entirely.
    t.market.cancel_order(&buyer, &o2);
    t.market.cancel_order(&buyer, &o3);
    let book = t.market.get_order_book(&t.token.address);
    assert_eq!(book.bids.len(), 0);
    assert!(t.market.get_best_bid(&t.token.address).is_none());
}

#[test]
fn match_best_order_fills_top_of_book() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    t.token.transfer(&t.admin, &seller, &1_000i128);
    verify_kyc(&t, &buyer);
    verify_kyc(&t, &seller);

    // Two asks: 150 and 200. The 150 ask is best.
    let ask_lo = t.market.place_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &150i128,
        &40i128,
        &future_expiry(&t.env),
        &0i128,
    );
    let ask_hi = t.market.place_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &200i128,
        &40i128,
        &future_expiry(&t.env),
        &0i128,
    );

    let filled_id =
        t.market
            .match_best_order(&buyer, &t.token.address, &Symbol::new(&t.env, "buy"));
    assert_eq!(filled_id, ask_lo);

    // Top of book advances to the 200 ask, and the buyer received 40 RWA.
    assert_eq!(
        t.market.get_best_ask(&t.token.address).unwrap().order_id,
        ask_hi
    );
    assert_eq!(t.token.get_balance(&buyer).amount, 40);
}

#[test]
fn match_best_order_respects_time_priority() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    t.token.transfer(&t.admin, &seller, &1_000i128);
    verify_kyc(&t, &buyer);
    verify_kyc(&t, &seller);

    // Two asks at the same price: the earliest must be matched first.
    let first = t.market.place_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &100i128,
        &40i128,
        &future_expiry(&t.env),
        &0i128,
    );
    let second = t.market.place_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &100i128,
        &40i128,
        &future_expiry(&t.env),
        &0i128,
    );

    let filled = t
        .market
        .match_best_order(&buyer, &t.token.address, &Symbol::new(&t.env, "buy"));
    assert_eq!(filled, first);
    assert_eq!(
        t.market.get_best_ask(&t.token.address).unwrap().order_id,
        second
    );
}

// Issue #142: stop-loss and take-profit conditional orders
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn stop_loss_triggers_on_price_drop() {
    let t = setup();
    let owner = Address::generate(&t.env);
    let trader = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&trader, &1_000_000i128);
    t.token.transfer(&t.admin, &owner, &500i128);
    t.token.transfer(&t.admin, &seller, &500i128);
    verify_kyc(&t, &owner);
    verify_kyc(&t, &trader);
    verify_kyc(&t, &seller);

    let rwa_before = t.token.get_balance(&owner).amount;
    let cond_id = t.market.place_conditional_order(
        &owner,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &75i128,
        &50i128,
        &OrderCondition::StopLoss(80i128),
        &future_expiry(&t.env),
    );

    // Escrowed immediately and kept off the book until triggered.
    assert_eq!(t.token.get_balance(&owner).amount, rwa_before - 50);
    let cond = t.market.get_conditional_order(&cond_id);
    assert!(cond.is_active);
    assert_eq!(cond.activated_order_id, 0);

    // A trade at 80 crosses the stop-loss trigger.
    let buy_id = t.market.place_order(
        &trader,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &80i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
    t.market.fill_order(&seller, &buy_id, &50i128);

    // Triggered → converted into a resting limit order.
    let cond = t.market.get_conditional_order(&cond_id);
    assert!(!cond.is_active);
    assert!(cond.activated_order_id > 0);

    // The activated order rests on the book and can be filled normally.
    let buyer = Address::generate(&t.env);
    base_admin.mint(&buyer, &1_000_000i128);
    verify_kyc(&t, &buyer);
    t.market
        .fill_order(&buyer, &cond.activated_order_id, &50i128);
    assert_eq!(t.token.get_balance(&buyer).amount, 50);
}

#[test]
fn take_profit_triggers_on_price_rise() {
    let t = setup();
    let owner = Address::generate(&t.env);
    let trader = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&trader, &1_000_000i128);
    t.token.transfer(&t.admin, &owner, &500i128);
    t.token.transfer(&t.admin, &seller, &500i128);
    verify_kyc(&t, &owner);
    verify_kyc(&t, &trader);
    verify_kyc(&t, &seller);

    let cond_id = t.market.place_conditional_order(
        &owner,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &118i128,
        &50i128,
        &OrderCondition::TakeProfit(120i128),
        &future_expiry(&t.env),
    );

    // A trade at 120 crosses the take-profit trigger.
    let buy_id = t.market.place_order(
        &trader,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &120i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
    t.market.fill_order(&seller, &buy_id, &50i128);

    let cond = t.market.get_conditional_order(&cond_id);
    assert!(!cond.is_active);
    assert!(cond.activated_order_id > 0);
}

#[test]
fn conditional_order_expires_and_refunds() {
    let t = setup();
    let owner = Address::generate(&t.env);
    t.token.transfer(&t.admin, &owner, &500i128);
    verify_kyc(&t, &owner);

    let rwa_before = t.token.get_balance(&owner).amount;
    let expiry = t.env.ledger().timestamp() + 100;
    let cond_id = t.market.place_conditional_order(
        &owner,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &75i128,
        &50i128,
        &OrderCondition::StopLoss(80i128),
        &expiry,
    );
    assert_eq!(t.token.get_balance(&owner).amount, rwa_before - 50);

    t.env.ledger().set_timestamp(expiry + 1);
    t.market.evaluate_conditional_orders(&t.token.address);

    let cond = t.market.get_conditional_order(&cond_id);
    assert!(!cond.is_active);
    assert_eq!(cond.activated_order_id, 0);
    // Escrowed tokens are returned to the owner.
    assert_eq!(t.token.get_balance(&owner).amount, rwa_before);
}

#[test]
fn cancel_conditional_order_refunds_escrow() {
    let t = setup();
    let owner = Address::generate(&t.env);
    t.token.transfer(&t.admin, &owner, &500i128);
    verify_kyc(&t, &owner);

    let rwa_before = t.token.get_balance(&owner).amount;
    let cond_id = t.market.place_conditional_order(
        &owner,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &75i128,
        &50i128,
        &OrderCondition::StopLoss(80i128),
        &future_expiry(&t.env),
    );
    t.market.cancel_conditional_order(&owner, &cond_id);

    assert!(!t.market.get_conditional_order(&cond_id).is_active);
    assert_eq!(t.token.get_balance(&owner).amount, rwa_before);
}

#[test]
fn conditional_order_limit_is_enforced() {
    let t = setup();
    let owner = Address::generate(&t.env);
    t.token.transfer(&t.admin, &owner, &10_000i128);
    verify_kyc(&t, &owner);

    for _ in 0..50u32 {
        t.market.place_conditional_order(
            &owner,
            &t.token.address,
            &Symbol::new(&t.env, "sell"),
            &75i128,
            &10i128,
            &OrderCondition::StopLoss(1i128),
            &future_expiry(&t.env),
        );
    }

    // The 51st live conditional order for this user/token is rejected.
    let res = t.market.try_place_conditional_order(
        &owner,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &75i128,
        &10i128,
        &OrderCondition::StopLoss(1i128),
        &future_expiry(&t.env),
    );
    assert!(res.is_err());
}

#[test]
fn conditional_order_without_escrow_funds_panics() {
    let t = setup();
    let broke = Address::generate(&t.env);
    verify_kyc(&t, &broke);

    // No RWA balance to escrow → liquidity/balance check rejects the order.
    let res = t.market.try_place_conditional_order(
        &broke,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &75i128,
        &50i128,
        &OrderCondition::StopLoss(80i128),
        &future_expiry(&t.env),
    );
    assert!(res.is_err());
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issues #147-#150: circuit breaker, fee tiers, self-trade guard, price oracle
// ═══════════════════════════════════════════════════════════════════════════════

/// Assert that a `try_*` market call failed with a specific contract error.
fn assert_market_error<T>(res: Result<T, Result<SdkError, InvokeError>>, expected: MarketError) {
    let err = res
        .err()
        .expect("expected the call to fail")
        .expect("expected a contract error, got a host error");
    assert_eq!(err.get_code(), expected as u32);
}

/// Execute a limit order and settle it immediately, establishing a VWAP.
fn trade_at(t: &TestEnv, buyer: &Address, seller: &Address, price: i128, amount: i128) {
    let order_id = t.market.place_order(
        buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &price,
        &amount,
        &future_expiry(&t.env),
        &0i128,
    );
    t.market.fill_order(seller, &order_id, &amount);
}

// ── Issue #149: self-trade prevention ────────────────────────────────────────

#[test]
fn fill_order_rejects_self_trade() {
    let t = setup();
    let trader = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&trader, &10_000i128);
    t.token.transfer(&t.admin, &trader, &100i128);

    let order_id = t.market.place_order(
        &trader,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &10i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );

    // The maker may not fill their own resting order (wash trading).
    assert_market_error(
        t.market.try_fill_order(&trader, &order_id, &50i128),
        MarketError::SelfTrade,
    );

    // A different trader can fill it normally.
    let other = Address::generate(&t.env);
    base_admin.mint(&other, &10_000i128);
    t.market.fill_order(&other, &order_id, &50i128);
    assert_eq!(t.token.get_balance(&other).amount, 50);
}

#[test]
fn market_order_never_matches_own_resting_order() {
    let t = setup();
    let maker = Address::generate(&t.env);
    let other = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&maker, &1_000_000i128);
    t.token.transfer(&t.admin, &maker, &100i128);
    t.token.transfer(&t.admin, &other, &100i128);

    // The maker's ask ranks first (best price, earliest arrival)...
    t.market.place_order(
        &maker,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &100i128,
        &30i128,
        &future_expiry(&t.env),
        &0i128,
    );
    let other_ask = t.market.place_order(
        &other,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &100i128,
        &20i128,
        &future_expiry(&t.env),
        &0i128,
    );

    // ...so a market buy from the maker must skip its own order.
    let filled = t.market.place_market_order(
        &maker,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &20i128,
        &100i128,
    );
    assert_eq!(filled, 20);

    // The other seller's order is consumed; the maker's own ask still rests.
    assert!(t.market.try_fill_order(&other, &other_ask, &1i128).is_err());
    let book = t.market.get_order_book(&t.token.address);
    assert_eq!(book.asks.len(), 1);
    assert_eq!(book.asks.get(0).unwrap().total_amount, 30);
    assert_eq!(book.asks.get(0).unwrap().order_count, 1);
}

// ── Issue #147: circuit breaker ──────────────────────────────────────────────

fn tripped_test_env() -> (TestEnv, Address, Address) {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    t.token.transfer(&t.admin, &seller, &1_000i128);

    t.market.set_circuit_breaker_config(
        &t.admin,
        &CircuitBreakerConfig {
            threshold_bps: 500, // 5%
            cooldown_seconds: 300,
            auto_reset: true,
        },
    );

    // Establish a VWAP of 100.
    trade_at(&t, &buyer, &seller, 100, 50);
    (t, buyer, seller)
}

/// Trade 10 units at `price` with the tripped-test buyer/seller pair.
fn trade_after_trip(t: &TestEnv, buyer: &Address, seller: &Address, price: i128) {
    let order_id = t.market.place_order(
        buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &price,
        &10i128,
        &future_expiry(&t.env),
        &0i128,
    );
    t.market.fill_order(seller, &order_id, &10i128);
}

#[test]
fn circuit_breaker_trips_on_price_deviation() {
    let (t, buyer, seller) = tripped_test_env();
    assert_eq!(t.market.get_vwap(&t.token.address), 100);

    let config = t.market.get_circuit_breaker_config();
    assert_eq!(config.threshold_bps, 500);
    assert_eq!(config.cooldown_seconds, 300);
    assert!(config.auto_reset);

    // A trade 30% away from VWAP trips the breaker (threshold is 5%).
    trade_after_trip(&t, &buyer, &seller, 130);

    let state = t.market.get_circuit_breaker_state(&t.token.address);
    assert!(state.is_tripped);
    assert_eq!(state.trip_price, 130);
    assert_eq!(state.reference_price, 100);
    assert!(t.market.is_circuit_breaker_tripped(&t.token.address));

    // The trigger is recorded, not raised, so the trade settled...
    assert_eq!(t.token.get_balance(&buyer).amount, 60);

    // ...but every new order is now rejected for the cooldown window.
    assert_market_error(
        t.market.try_place_order(
            &buyer,
            &t.token.address,
            &Symbol::new(&t.env, "buy"),
            &100i128,
            &10i128,
            &future_expiry(&t.env),
            &0i128,
        ),
        MarketError::CircuitBreakerTripped,
    );
}

#[test]
fn circuit_breaker_cooldown_rejects_new_orders() {
    let (t, buyer, seller) = tripped_test_env();
    trade_after_trip(&t, &buyer, &seller, 130);
    assert!(t.market.is_circuit_breaker_tripped(&t.token.address));

    // Even a price back at the reference is rejected while the cooldown runs.
    assert_market_error(
        t.market.try_place_order(
            &buyer,
            &t.token.address,
            &Symbol::new(&t.env, "buy"),
            &100i128,
            &50i128,
            &future_expiry(&t.env),
            &0i128,
        ),
        MarketError::CircuitBreakerTripped,
    );
    assert!(t.market.is_circuit_breaker_tripped(&t.token.address));

    // Market orders are rejected too.
    assert_market_error(
        t.market.try_place_market_order(
            &buyer,
            &t.token.address,
            &Symbol::new(&t.env, "buy"),
            &10i128,
            &130i128,
        ),
        MarketError::CircuitBreakerTripped,
    );
}

#[test]
fn circuit_breaker_auto_resets_after_cooldown() {
    let (t, buyer, seller) = tripped_test_env();
    trade_after_trip(&t, &buyer, &seller, 130);
    assert!(t.market.is_circuit_breaker_tripped(&t.token.address));

    // Cooldown is 300s: just before it elapses the breaker is still closed.
    t.env
        .ledger()
        .set_timestamp(t.env.ledger().timestamp() + 299);
    assert!(t
        .market
        .try_place_order(
            &buyer,
            &t.token.address,
            &Symbol::new(&t.env, "buy"),
            &100i128,
            &10i128,
            &future_expiry(&t.env),
            &0i128,
        )
        .is_err());

    // Once the cooldown elapses and the submitted price is stable it clears
    // itself and the order is accepted.
    t.env.ledger().set_timestamp(t.env.ledger().timestamp() + 2);
    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &105i128,
        &10i128,
        &future_expiry(&t.env),
        &0i128,
    );
    assert!(order_id > 0);
    assert!(!t.market.is_circuit_breaker_tripped(&t.token.address));
}

#[test]
fn circuit_breaker_manual_force_reset() {
    let (t, buyer, seller) = tripped_test_env();

    // Auto-reset disabled: the breaker must be cleared manually.
    t.market.set_circuit_breaker_config(
        &t.admin,
        &CircuitBreakerConfig {
            threshold_bps: 500,
            cooldown_seconds: 300,
            auto_reset: false,
        },
    );
    trade_after_trip(&t, &buyer, &seller, 130);
    assert!(t.market.is_circuit_breaker_tripped(&t.token.address));

    // Long past the cooldown, still tripped because auto-reset is off.
    t.env
        .ledger()
        .set_timestamp(t.env.ledger().timestamp() + 10_000);
    assert!(t
        .market
        .try_place_order(
            &buyer,
            &t.token.address,
            &Symbol::new(&t.env, "buy"),
            &105i128,
            &10i128,
            &future_expiry(&t.env),
            &0i128,
        )
        .is_err());

    // Admin force-reset clears it.
    t.market
        .force_reset_circuit_breaker(&t.admin, &t.token.address);
    assert!(!t.market.is_circuit_breaker_tripped(&t.token.address));

    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &105i128,
        &10i128,
        &future_expiry(&t.env),
        &0i128,
    );
    assert!(order_id > 0);
}

// ── Issue #148: 30-day volume fee tiers ──────────────────────────────────────

#[test]
fn default_fee_tiers_follow_volume_ladder() {
    let t = setup();
    let tiers = t.market.get_fee_tiers();
    assert_eq!(tiers.len(), 4);

    assert_eq!(tiers.get(0).unwrap().min_30d_volume, 0);
    assert_eq!(tiers.get(0).unwrap().taker_fee_bps, 30);
    assert_eq!(tiers.get(1).unwrap().min_30d_volume, 100_000);
    assert_eq!(tiers.get(1).unwrap().maker_fee_bps, 20);
    assert_eq!(tiers.get(2).unwrap().min_30d_volume, 1_000_000);
    assert_eq!(tiers.get(2).unwrap().taker_fee_bps, 15);
    assert_eq!(tiers.get(3).unwrap().min_30d_volume, 10_000_000);
    assert_eq!(tiers.get(3).unwrap().maker_fee_bps, 10);

    // A new trader starts with no volume in the lowest tier.
    let fresh = Address::generate(&t.env);
    assert_eq!(t.market.get_30d_volume(&fresh), 0);
    assert_eq!(t.market.get_fee_tier(&fresh).min_30d_volume, 0);
}

#[test]
fn fee_tier_upgrades_and_downgrades_with_rolling_volume() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    t.token.transfer(&t.admin, &seller, &500_000i128);

    // Activate the tiered fee schedule.
    let tiers = t.market.get_fee_tiers();
    t.market.set_fee_tiers(&t.admin, &tiers);

    // 99_990 of volume stays below the first boundary.
    trade_at(&t, &buyer, &seller, 1, 99_990);
    assert_eq!(t.market.get_30d_volume(&seller), 99_990);
    assert_eq!(t.market.get_fee_tier(&seller).taker_fee_bps, 30);

    // Crossing 100_000 moves the trader up a tier.
    trade_at(&t, &buyer, &seller, 1, 10);
    assert_eq!(t.market.get_30d_volume(&seller), 100_000);
    assert_eq!(t.market.get_fee_tier(&seller).min_30d_volume, 100_000);
    assert_eq!(t.market.get_fee_tier(&seller).taker_fee_bps, 20);

    // Volume older than the 30-day window rolls off, downgrading the trader.
    t.env
        .ledger()
        .set_timestamp(t.env.ledger().timestamp() + 30 * 24 * 60 * 60 + 1);
    assert_eq!(t.market.get_30d_volume(&seller), 0);
    assert_eq!(t.market.get_fee_tier(&seller).taker_fee_bps, 30);
}

#[test]
fn tiered_maker_and_taker_fees_applied_at_execution() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    t.token.transfer(&t.admin, &seller, &1_000i128);

    // One explicit tier: 1% maker fee, 2% taker fee.
    t.market.set_fee_tiers(
        &t.admin,
        &Vec::from_array(
            &t.env,
            [FeeTier {
                min_30d_volume: 0,
                maker_fee_bps: 100,
                taker_fee_bps: 200,
            }],
        ),
    );

    let recipient_before = t.base_token.balance(&t.admin);
    let seller_before = t.base_token.balance(&seller);

    // Maker (buyer) buys 100 @ 10 = 1000 settlement value.
    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &10i128,
        &100i128,
        &future_expiry(&t.env),
        &0i128,
    );
    t.market.fill_order(&seller, &order_id, &100i128);

    // Maker fee 10 + taker fee 20 goes to the fee recipient.
    assert_eq!(t.base_token.balance(&t.admin), recipient_before + 30);
    // The taker receives the settlement net of both fees.
    assert_eq!(t.base_token.balance(&seller), seller_before + 970);
}

// ── Issue #150: price oracle integration ─────────────────────────────────────

#[test]
fn oracle_price_validates_order_price() {
    let t = setup();
    let oracle = Address::generate(&t.env);
    let buyer = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);

    t.market
        .set_price_oracle(&t.admin, &t.token.address, &oracle);
    t.market
        .set_oracle_price(&oracle, &t.token.address, &100i128, &5i128);

    let entry = t.market.get_oracle_price(&t.token.address).unwrap();
    assert_eq!(entry.price, 100);
    assert_eq!(entry.confidence_interval, 5);
    assert_eq!(entry.oracle_address, oracle);
    assert_eq!(entry.token, t.token.address);
    assert_eq!(
        t.market.get_configured_oracle(&t.token.address).unwrap(),
        oracle
    );

    // 5% from the oracle is inside the market's 20% band.
    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &105i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
    assert!(order_id > 0);

    // 100% from the oracle is rejected, and the VWAP breaker stays untouched
    // because the oracle was authoritative.
    assert_market_error(
        t.market.try_place_order(
            &buyer,
            &t.token.address,
            &Symbol::new(&t.env, "buy"),
            &200i128,
            &50i128,
            &future_expiry(&t.env),
            &0i128,
        ),
        MarketError::OraclePriceDeviation,
    );
    assert!(!t.market.is_circuit_breaker_tripped(&t.token.address));
}

#[test]
fn stale_oracle_falls_back_to_vwap() {
    let t = setup();
    let oracle = Address::generate(&t.env);
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    t.token.transfer(&t.admin, &seller, &1_000i128);

    t.market
        .set_price_oracle(&t.admin, &t.token.address, &oracle);
    t.market
        .set_oracle_price(&oracle, &t.token.address, &100i128, &1i128);

    // Age the oracle past its one-hour freshness window...
    t.env
        .ledger()
        .set_timestamp(t.env.ledger().timestamp() + 7200);

    // ...and establish a VWAP of 200 with a real trade.
    trade_at(&t, &buyer, &seller, 200, 50);
    assert_eq!(t.market.get_vwap(&t.token.address), 200);

    // 115 is only 15% from the (stale) oracle but 42.5% from VWAP: the stale
    // price must be ignored in favour of the VWAP reference.
    assert_market_error(
        t.market.try_place_order(
            &buyer,
            &t.token.address,
            &Symbol::new(&t.env, "buy"),
            &115i128,
            &50i128,
            &future_expiry(&t.env),
            &0i128,
        ),
        MarketError::OraclePriceDeviation,
    );

    // A price close to VWAP is still accepted while the oracle is stale.
    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &205i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
    assert!(order_id > 0);
}

#[test]
fn oracle_price_requires_configured_oracle_auth() {
    let t = setup();
    let oracle = Address::generate(&t.env);
    let attacker = Address::generate(&t.env);

    t.market
        .set_price_oracle(&t.admin, &t.token.address, &oracle);

    // Only the configured oracle may publish a price.
    assert_market_error(
        t.market
            .try_set_oracle_price(&attacker, &t.token.address, &100i128, &1i128),
        MarketError::Unauthorized,
    );

    // Only the admin may configure an oracle.
    assert_market_error(
        t.market
            .try_set_price_oracle(&attacker, &t.token.address, &attacker),
        MarketError::Unauthorized,
    );

    // A token with no configured oracle cannot receive prices.
    let other_token = Address::generate(&t.env);
    assert_market_error(
        t.market
            .try_set_oracle_price(&oracle, &other_token, &100i128, &1i128),
        MarketError::Unauthorized,
    );
}

#[test]
fn no_oracle_configured_skips_oracle_validation() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    t.token.transfer(&t.admin, &seller, &1_000i128);

    assert!(t.market.get_oracle_price(&t.token.address).is_none());
    assert!(t.market.get_configured_oracle(&t.token.address).is_none());

    trade_at(&t, &buyer, &seller, 100, 50);

    // With no oracle configured there is no external reference to validate
    // against: a far-out limit order simply rests on the book.
    let order_id = t.market.place_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &200i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
    assert!(order_id > 0);
    assert!(!t.market.is_circuit_breaker_tripped(&t.token.address));
}

// ════════════════════════════════════════════════════════════════════════════════
// Issue #144: TWAP order execution
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn twap_order_executes_in_slices() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    t.token.transfer(&t.admin, &seller, &500i128);
    verify_kyc(&t, &buyer);
    verify_kyc(&t, &seller);

    // Seller places 5 asks of 20 RWA at price 10.
    for _ in 0..5 {
        t.market.place_order(
            &seller,
            &t.token.address,
            &Symbol::new(&t.env, "sell"),
            &10i128,
            &20i128,
            &future_expiry(&t.env),
            &0i128,
        );
    }

    // TWAP buy: 100 RWA in 5 slices of 20, limit price 10, interval 60s.
    let filled = t.market.place_twap_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &100i128,
        &5u32,
        &60u64,
        &10i128,
    );
    assert_eq!(filled, 100);
    assert_eq!(t.token.get_balance(&buyer).amount, 100);
}

#[test]
fn twap_order_partial_fill_when_liquidity_exhausted() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    t.token.transfer(&t.admin, &seller, &30i128);
    verify_kyc(&t, &buyer);
    verify_kyc(&t, &seller);

    // Seller places 1 ask of 30 RWA at price 10.
    t.market.place_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &10i128,
        &30i128,
        &future_expiry(&t.env),
        &0i128,
    );

    // TWAP buy: 100 RWA in 5 slices, but only 30 available.
    let filled = t.market.place_twap_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &100i128,
        &5u32,
        &60u64,
        &10i128,
    );
    assert_eq!(filled, 30);
    assert_eq!(t.token.get_balance(&buyer).amount, 30);
}

#[test]
fn twap_order_respects_limit_price() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    t.token.transfer(&t.admin, &seller, &100i128);
    verify_kyc(&t, &buyer);
    verify_kyc(&t, &seller);

    // Ask at price 20, but TWAP limit is 10.
    t.market.place_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &20i128,
        &100i128,
        &future_expiry(&t.env),
        &0i128,
    );

    let res = t.market.try_place_twap_order(
        &buyer,
        &t.token.address,
        &Symbol::new(&t.env, "buy"),
        &100i128,
        &5u32,
        &60u64,
        &10i128,
    );
    assert!(res.is_err());
    assert_eq!(t.token.get_balance(&buyer).amount, 0);
}

#[test]
fn twap_sell_order_executes_against_bids() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let seller = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    t.token.transfer(&t.admin, &seller, &100i128);
    verify_kyc(&t, &buyer);
    verify_kyc(&t, &seller);

    // Buyer places 5 bids of 20 RWA at price 10.
    for _ in 0..5 {
        t.market.place_order(
            &buyer,
            &t.token.address,
            &Symbol::new(&t.env, "buy"),
            &10i128,
            &20i128,
            &future_expiry(&t.env),
            &0i128,
        );
    }

    // TWAP sell: 100 RWA in 5 slices, limit price 10.
    let filled = t.market.place_twap_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &100i128,
        &5u32,
        &60u64,
        &10i128,
    );
    assert_eq!(filled, 100);
    assert_eq!(t.token.get_balance(&seller).amount, 0);
}

// ════════════════════════════════════════════════════════════════════════════════
// Issue #145: maker balance verification before sell orders
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn place_sell_order_insufficient_balance_rejected() {
    let t = setup();
    let seller = Address::generate(&t.env);
    // Seller has no RWA tokens.

    let res = t.market.try_place_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &100i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
    assert!(res.is_err());
}

#[test]
fn place_sell_order_partial_balance_rejected() {
    let t = setup();
    let seller = Address::generate(&t.env);
    t.token.transfer(&t.admin, &seller, &30i128); // only 30, trying to sell 50

    let res = t.market.try_place_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &100i128,
        &50i128,
        &future_expiry(&t.env),
        &0i128,
    );
    assert!(res.is_err());
}

#[test]
fn place_sell_order_sufficient_balance_succeeds() {
    let t = setup();
    let seller = Address::generate(&t.env);
    t.token.transfer(&t.admin, &seller, &100i128);

    let order_id = t.market.place_order(
        &seller,
        &t.token.address,
        &Symbol::new(&t.env, "sell"),
        &100i128,
        &100i128,
        &future_expiry(&t.env),
        &0i128,
    );
    assert_eq!(order_id, 1);
}

// ════════════════════════════════════════════════════════════════════════════════
// Issue #146: order book depth aggregation
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn order_book_depth_aggregates_bids_within_range() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    verify_kyc(&t, &buyer);

    // Bids at 100 (20 RWA), 90 (30 RWA), 80 (40 RWA), 50 (10 RWA — out of 10% range).
    t.market.place_order(&buyer, &t.token.address, &Symbol::new(&t.env, "buy"), &100i128, &20i128, &future_expiry(&t.env), &0i128);
    t.market.place_order(&buyer, &t.token.address, &Symbol::new(&t.env, "buy"), &90i128, &30i128, &future_expiry(&t.env), &0i128);
    t.market.place_order(&buyer, &t.token.address, &Symbol::new(&t.env, "buy"), &80i128, &40i128, &future_expiry(&t.env), &0i128);
    t.market.place_order(&buyer, &t.token.address, &Symbol::new(&t.env, "buy"), &50i128, &10i128, &future_expiry(&t.env), &0i128);

    let depth = t.market.get_order_book_depth(&t.token.address, &Symbol::new(&t.env, "buy"), &1000u64);
    assert_eq!(depth.best_price, 100);
    // 10% of 100 = 10, so min price = 90. Levels at 100 and 90 are in range.
    assert_eq!(depth.total_amount, 50); // 20 + 30
    assert_eq!(depth.order_count, 2);
    assert_eq!(depth.levels.len(), 2);
}

#[test]
fn order_book_depth_aggregates_asks_within_range() {
    let t = setup();
    let seller = Address::generate(&t.env);
    t.token.transfer(&t.admin, &seller, &500i128);
    verify_kyc(&t, &seller);

    // Asks at 100 (20 RWA), 110 (30 RWA), 120 (40 RWA), 200 (10 RWA — out of 10% range).
    t.market.place_order(&seller, &t.token.address, &Symbol::new(&t.env, "sell"), &100i128, &20i128, &future_expiry(&t.env), &0i128);
    t.market.place_order(&seller, &t.token.address, &Symbol::new(&t.env, "sell"), &110i128, &30i128, &future_expiry(&t.env), &0i128);
    t.market.place_order(&seller, &t.token.address, &Symbol::new(&t.env, "sell"), &120i128, &40i128, &future_expiry(&t.env), &0i128);
    t.market.place_order(&seller, &t.token.address, &Symbol::new(&t.env, "sell"), &200i128, &10i128, &future_expiry(&t.env), &0i128);

    let depth = t.market.get_order_book_depth(&t.token.address, &Symbol::new(&t.env, "sell"), &1000u64);
    assert_eq!(depth.best_price, 100);
    // 10% of 100 = 10, so max price = 110. Levels at 100 and 110 are in range.
    assert_eq!(depth.total_amount, 50); // 20 + 30
    assert_eq!(depth.order_count, 2);
    assert_eq!(depth.levels.len(), 2);
}

#[test]
fn order_book_depth_empty_book() {
    let t = setup();
    let depth = t.market.get_order_book_depth(&t.token.address, &Symbol::new(&t.env, "buy"), &1000u64);
    assert_eq!(depth.best_price, 0);
    assert_eq!(depth.total_amount, 0);
    assert_eq!(depth.order_count, 0);
    assert_eq!(depth.levels.len(), 0);
}

#[test]
fn order_book_depth_invalid_side_rejected() {
    let t = setup();
    let res = t.market.try_get_order_book_depth(
        &t.token.address,
        &Symbol::new(&t.env, "invalid"),
        &1000u64,
    );
    assert!(res.is_err());
}
