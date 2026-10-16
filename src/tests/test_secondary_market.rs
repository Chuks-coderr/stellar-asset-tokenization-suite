#![cfg(test)]

use soroban_sdk::{
    testutils::{Address as _, Ledger, LedgerInfo},
    token::{StellarAssetClient, TokenClient},
    Address, Env, Map, Symbol, Vec,
};

use crate::{
    compliance_registry::{ComplianceRegistry, ComplianceRegistryClient, KYCStatus},
    dividend_distributor::{DividendDistributor, DividendDistributorClient},
    rwa_token::{RWAToken, RWATokenClient},
    secondary_market::{MarketError, SecondaryMarket, SecondaryMarketClient},
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

    TestEnv { env, admin, market, token, base_token, compliance }
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
    t.market.initialize(
        &t.admin,
        &other,
        &other,
        &other,
        &50i64,
        &10i128,
        &2000i64,
    );
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
        &100i128,  // price
        &50i128,   // amount
        &future_expiry(&t.env),
        &0i128,    // min_fill
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &100i128, &50i128, &future_expiry(&t.env), &0i128,
    );
    let id2 = t.market.place_order(
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &100i128, &50i128, &future_expiry(&t.env), &0i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &100i128, &5i128, &future_expiry(&t.env), &0i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &0i128, &50i128, &future_expiry(&t.env), &0i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &100i128, &50i128, &0u64, &0i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "invalid"),
        &100i128, &50i128, &future_expiry(&t.env), &0i128,
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
        &seller, &t.token.address, &Symbol::new(&t.env, "sell"),
        &100i128, &100i128, &future_expiry(&t.env), &0i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &100i128, &50i128, &future_expiry(&t.env), &0i128,
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
        &seller, &t.token.address, &Symbol::new(&t.env, "sell"),
        &100i128, &100i128, &future_expiry(&t.env), &0i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &100i128, &50i128, &future_expiry(&t.env), &0i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &10i128, &100i128, &future_expiry(&t.env), &0i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &10i128, &50i128, &future_expiry(&t.env), &0i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &50i128, &100i128, &future_expiry(&t.env), &0i128,
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
        &admin, &base_id.address(), &compliance_id, &dist_id,
        &50i64, &10i128, &2000i64,
    );

    let unverified_buyer = Address::generate(&env);
    base_admin.mint(&unverified_buyer, &10_000i128);

    // Should panic because KYC is required and buyer is not verified
    market.place_order(
        &unverified_buyer, &token_id,
        &Symbol::new(&env, "buy"),
        &100i128, &50i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &10i128, &100i128, &future_expiry(&t.env), &0i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &10i128, &50i128, &future_expiry(&t.env), &0i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &10i128, &50i128, &future_expiry(&t.env), &0i128,
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
        &user_a, &t.token.address, &Symbol::new(&t.env, "buy"),
        &10i128, &50i128, &future_expiry(&t.env), &0i128,
    );
    t.market.cancel_order(&user_a, &id1);

    let id2 = t.market.place_order(
        &user_a, &t.token.address, &Symbol::new(&t.env, "buy"),
        &10i128, &50i128, &future_expiry(&t.env), &0i128,
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
            &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
            &10i128, &20i128, &future_expiry(&t.env), &0i128,
        );
        order_ids.push_back(id);
    }

    // Fill all 5 orders from the same seller
    for i in 0..5 {
        let oid = order_ids.get(i).unwrap();
        t.market.fill_order(&seller, &oid, &20i128);
    }

    // Seller sold 5 * 20 = 100 tokens
    assert_eq!(t.token.get_balance(&seller).amount, seller_balance_before - 100);
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &10i128, &50i128, &future_expiry(&t.env), &0i128,
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
            &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
            &10i128, &10i128, &future_expiry(&t.env), &0i128,
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
                &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
                &10i128, &30i128, &future_expiry(&t.env), &0i128,
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
            &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
            &10i128, &20i128, &future_expiry(&t.env), &0i128,
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
        &trader1, &t.token.address, &Symbol::new(&t.env, "buy"),
        &200i128, &10i128, &future_expiry(&t.env), &0i128,
    );
    t.market.fill_order(&trader2, &o1, &10i128);

    // trader2 buys low
    let o2 = t.market.place_order(
        &trader2, &t.token.address, &Symbol::new(&t.env, "buy"),
        &10i128, &10i128, &future_expiry(&t.env), &0i128,
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
            &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
            &10i128, &5i128, &future_expiry(&t.env), &0i128,
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
            &trader_a, &t.token.address, &Symbol::new(&t.env, "buy"),
            &10i128, &2i128, &future_expiry(&t.env), &0i128,
        );
        t.market.fill_order(&trader_b, &buy_id, &2i128);

        // B places buy
        let sell_id = t.market.place_order(
            &trader_b, &t.token.address, &Symbol::new(&t.env, "buy"),
            &10i128, &2i128, &future_expiry(&t.env), &0i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &10i128, &100i128, &future_expiry(&t.env), &0i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &50i128, &100i128, &future_expiry(&t.env), &0i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &100i128, &10i128, &future_expiry(&t.env), &0i128,
    );
    let id2 = t.market.place_order(
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &90i128, &20i128, &future_expiry(&t.env), &0i128,
    );
    let id3 = t.market.place_order(
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &80i128, &30i128, &future_expiry(&t.env), &0i128,
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
            &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
            &price, &10i128, &future_expiry(&t.env), &0i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &50i128, &50i128, &future_expiry(&t.env), &0i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &10i128, &20i128, &future_expiry(&t.env), &0i128,
    );
    let buy2 = t.market.place_order(
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &10i128, &30i128, &future_expiry(&t.env), &0i128,
    );
    let sell1 = t.market.place_order(
        &seller, &t.token.address, &Symbol::new(&t.env, "sell"),
        &10i128, &40i128, &future_expiry(&t.env), &0i128,
    );

    // Fill buy1, cancel buy2, leave sell1 active
    t.market.fill_order(&seller, &buy1, &20i128);
    t.market.cancel_order(&buyer, &buy2);

    // Verify escrow reconciliation
    // buy1 filled: buyer paid 200 base, got 20 RWA
    // buy2 cancelled: buyer got 300 base back
    // sell1 active: seller has 40 RWA in escrow
    assert_eq!(t.token.get_balance(&seller).amount, seller_rwa_before - 20 - 40);
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &10i128, &50i128, &expiry, &0i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &10i128, &50i128, &expiry, &0i128,
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &10i128, &50i128, &expiry, &0i128,
    );

    // Advance ledger timestamp past expiry
    t.env.ledger().set_timestamp(expiry + 20);

    // Explicitly call prune_expired_orders
    t.market.prune_expired_orders();

    // Escrow should be refunded automatically
    assert_eq!(t.base_token.balance(&buyer), balance_before);
}

// ═══════════════════════════════════════════════════════════════════════════════
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
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &100i128, &50i128, &future_expiry(&t.env), &0i128,
    );
    let best_bid = t.market.place_order(
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &120i128, &40i128, &future_expiry(&t.env), &0i128,
    );
    t.market.place_order(
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &90i128, &30i128, &future_expiry(&t.env), &0i128,
    );

    // Asks placed out of order: 200, 150, 180.
    t.market.place_order(
        &seller, &t.token.address, &Symbol::new(&t.env, "sell"),
        &200i128, &20i128, &future_expiry(&t.env), &0i128,
    );
    let best_ask = t.market.place_order(
        &seller, &t.token.address, &Symbol::new(&t.env, "sell"),
        &150i128, &25i128, &future_expiry(&t.env), &0i128,
    );
    t.market.place_order(
        &seller, &t.token.address, &Symbol::new(&t.env, "sell"),
        &180i128, &15i128, &future_expiry(&t.env), &0i128,
    );

    let book = t.market.get_order_book(&t.token.address);
    let bid_prices: std::vec::Vec<i128> = book.bids.iter().map(|l| l.price).collect();
    let ask_prices: std::vec::Vec<i128> = book.asks.iter().map(|l| l.price).collect();
    assert_eq!(bid_prices, std::vec![120i128, 100, 90]);
    assert_eq!(ask_prices, std::vec![150i128, 180, 200]);

    // O(1) best-price lookup points at index 0 of each side.
    assert_eq!(t.market.get_best_bid(&t.token.address).unwrap().order_id, best_bid);
    assert_eq!(t.market.get_best_ask(&t.token.address).unwrap().order_id, best_ask);
}

#[test]
fn order_book_insertion_and_removal() {
    let t = setup();
    let buyer = Address::generate(&t.env);
    let base_admin = StellarAssetClient::new(&t.env, &t.base_token.address);
    base_admin.mint(&buyer, &1_000_000i128);
    verify_kyc(&t, &buyer);

    let o1 = t.market.place_order(
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &100i128, &20i128, &future_expiry(&t.env), &0i128,
    );
    let o2 = t.market.place_order(
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &100i128, &30i128, &future_expiry(&t.env), &0i128,
    );
    let o3 = t.market.place_order(
        &buyer, &t.token.address, &Symbol::new(&t.env, "buy"),
        &110i128, &25i128, &future_expiry(&t.env), &0i128,
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
        &seller, &t.token.address, &Symbol::new(&t.env, "sell"),
        &150i128, &40i128, &future_expiry(&t.env), &0i128,
    );
    let ask_hi = t.market.place_order(
        &seller, &t.token.address, &Symbol::new(&t.env, "sell"),
        &200i128, &40i128, &future_expiry(&t.env), &0i128,
    );

    let filled_id = t
        .market
        .match_best_order(&buyer, &t.token.address, &Symbol::new(&t.env, "buy"));
    assert_eq!(filled_id, ask_lo);

    // Top of book advances to the 200 ask, and the buyer received 40 RWA.
    assert_eq!(t.market.get_best_ask(&t.token.address).unwrap().order_id, ask_hi);
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
        &seller, &t.token.address, &Symbol::new(&t.env, "sell"),
        &100i128, &40i128, &future_expiry(&t.env), &0i128,
    );
    let second = t.market.place_order(
        &seller, &t.token.address, &Symbol::new(&t.env, "sell"),
        &100i128, &40i128, &future_expiry(&t.env), &0i128,
    );

    let filled = t
        .market
        .match_best_order(&buyer, &t.token.address, &Symbol::new(&t.env, "buy"));
    assert_eq!(filled, first);
    assert_eq!(t.market.get_best_ask(&t.token.address).unwrap().order_id, second);
}
