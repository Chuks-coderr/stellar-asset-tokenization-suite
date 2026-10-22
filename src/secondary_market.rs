use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, log, panic_with_error, token::TokenClient,
    Address, Env, Map, Symbol, Vec,
};

use crate::compliance_registry::ComplianceRegistryClient;
use crate::dividend_distributor::DividendDistributorClient;
use crate::rwa_token::RWATokenClient;
// Issue #150: shared price-oracle helpers live in the custody validator so all
// valuation paths interpret oracle freshness/deviation identically.
use crate::custody_validator::{is_oracle_price_fresh, price_deviation_bps};

const STORAGE_VERSION: u32 = 1;

/// Issue #142: maximum live conditional orders per user and token.
const MAX_CONDITIONAL_ORDERS_PER_USER: u32 = 50;

/// Issue #148: rolling window used for the volume-based fee tiers (30 days).
const VOLUME_WINDOW_SECONDS: u64 = 30 * 24 * 60 * 60;

/// Issue #150: an oracle price older than this is considered stale and the
/// market falls back to its own VWAP reference.
const ORACLE_STALE_SECONDS: u64 = 3600;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum MarketError {
    Unauthorized = 1,
    InvalidOrder = 2,
    InsufficientBalance = 3,
    OrderNotFound = 4,
    OrderExpired = 5,
    InsufficientLiquidity = 6,
    TradingPaused = 7,
    ComplianceFailed = 8,
    CircuitBreakerTripped = 9,
    DividendHalt = 10,
    MinOrderSizeNotMet = 11,
    AlreadyInitialized = 12,
    StorageOutdated = 13,
    MarketNotInitialized = 14,
    AlreadyAtLatestVersion = 15,
    InvalidParameters = 16,
    // Issue #140: market orders.
    SlippageExceeded = 17,
    // Issue #142: conditional orders.
    ConditionalOrderLimitReached = 18,
    ConditionalOrderNotFound = 19,
    // Issue #149: wash trading protection.
    SelfTrade = 20,
    // Issue #150: oracle price validation.
    OraclePriceDeviation = 21,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Order {
    pub id: u64,
    pub maker: Address,
    pub token_address: Address,
    pub side: Symbol, // "buy" or "sell"
    pub price: i128,
    pub amount: i128,
    pub filled_amount: i128,
    pub min_fill: i128,
    pub expires_at: u64,
    pub created_at: u64,
    pub is_active: bool,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Trade {
    pub order_id: u64,
    pub taker: Address,
    pub fill_amount: i128,
    pub fill_price: i128,
    pub timestamp: u64,
}

/// Trigger condition for a stop-loss / take-profit order (issue #142).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OrderCondition {
    None,
    /// Triggers when the last traded price falls to or below the threshold.
    StopLoss(i128),
    /// Triggers when the last traded price rises to or above the threshold.
    TakeProfit(i128),
}

/// An off-book order held until its trigger condition is met (issue #142).
#[contracttype]
#[derive(Clone, Debug)]
pub struct ConditionalOrder {
    pub id: u64,
    pub owner: Address,
    pub token_address: Address,
    pub side: Symbol,
    pub price: i128,
    pub amount: i128,
    pub filled_amount: i128,
    pub condition: OrderCondition,
    pub expires_at: u64,
    pub created_at: u64,
    pub is_active: bool,
    /// The resting order id created when the condition triggered (0 otherwise).
    pub activated_order_id: u64,
}

/// One aggregated price level of the order book (issue #141).
#[contracttype]
#[derive(Clone, Debug)]
pub struct OrderBookLevel {
    pub price: i128,
    pub total_amount: i128,
    pub order_count: u32,
}

/// Aggregated, price-time-priority order book for a token (issue #141).
#[contracttype]
#[derive(Clone, Debug)]
pub struct OrderBook {
    /// Buy side, sorted by price descending (best first).
    pub bids: Vec<OrderBookLevel>,
    /// Sell side, sorted by price ascending (best first).
    pub asks: Vec<OrderBookLevel>,
}

/// A single resting order at the top of the book (issue #141).
#[contracttype]
#[derive(Clone, Debug)]
pub struct BookOrder {
    pub order_id: u64,
    pub maker: Address,
    pub side: Symbol,
    pub price: i128,
    pub remaining: i128,
    pub created_at: u64,
}

/// Aggregated order-book depth within a price range (issue #146).
#[contracttype]
#[derive(Clone, Debug)]
pub struct OrderBookDepth {
    pub best_price: i128,
    pub total_amount: i128,
    pub order_count: u32,
    pub levels: Vec<OrderBookLevel>,
}

/// Issue #147: parameters of the price-deviation circuit breaker.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CircuitBreakerConfig {
    /// Deviation from the reference price (in bps) that trips the breaker.
    pub threshold_bps: i64,
    /// How long new orders are rejected after the breaker trips.
    pub cooldown_seconds: u64,
    /// When true the breaker clears itself once the cooldown elapses and the
    /// market has stabilised; otherwise an admin must force a reset.
    pub auto_reset: bool,
}

/// Issue #147: current state of a token's circuit breaker.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CircuitBreakerState {
    pub is_tripped: bool,
    pub tripped_at: u64,
    /// Reference (VWAP) price the breaker was measured against when it tripped.
    pub reference_price: i128,
    /// Price whose deviation tripped the breaker.
    pub trip_price: i128,
}

/// Issue #148: a single maker/taker fee tier selected by 30-day volume.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeeTier {
    /// Minimum 30-day volume (in base currency) required for this tier.
    pub min_30d_volume: i128,
    pub maker_fee_bps: i64,
    pub taker_fee_bps: i64,
}

/// Issue #150: a price published by an external oracle.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OraclePrice {
    pub token: Address,
    pub price: i128,
    pub timestamp: u64,
    pub oracle_address: Address,
    pub confidence_interval: i128,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct MarketConfig {
    pub admin: Address,
    pub fee_rate_bps: i64,
    pub fee_recipient: Address,
    pub min_order_size: i128,
    pub max_price_deviation_bps: i64, // e.g. 2000 for 20%
    pub is_paused: bool,
    pub base_currency: Address,
    pub compliance_registry: Address,
    pub dividend_distributor: Address,
}

#[contracttype]
pub enum DataKey {
    Config,
    OrderCount,
    TradeCount,
    Order(u64),
    PriceHistory(Address), // Vec<Trade> or similar for VWAP
    VWAP(Address),
    LastUpdate(Address),
    // Issue #142: conditional orders.
    LastPrice(Address),                     // last traded price for a token
    ConditionalCount,                       // u64 number of conditional orders
    ConditionalOrder(u64),                  // ConditionalOrder by id
    UserConditionalCount(Address, Address), // (owner, token) -> u32 active count
    // Issue #141: sorted order-book indexes.
    BidPrices(Address),       // Vec<i128>, descending
    AskPrices(Address),       // Vec<i128>, ascending
    BidOrders(Address, i128), // Vec<u64> order ids at a bid price level
    AskOrders(Address, i128), // Vec<u64> order ids at an ask price level
    // Issue #147: circuit breaker.
    CircuitBreakerConfig,         // CircuitBreakerConfig
    CircuitBreakerState(Address), // CircuitBreakerState per token
    // Issue #148: volume-based fee tiers.
    FeeTiers,               // Vec<FeeTier>, ascending by min_30d_volume
    VolumeHistory(Address), // Vec<(timestamp, volume)> per trader
    TraderFeeBps(Address),  // i64 last applied fee (bps) per trader
    // Issue #150: price oracles.
    PriceOracle(Address), // configured oracle address per token
    OraclePrice(Address), // latest OraclePrice per token
}

#[contract]
pub struct SecondaryMarket;

#[contractimpl]
impl SecondaryMarket {
    pub fn initialize(
        env: Env,
        admin: Address,
        base_currency: Address,
        compliance_registry: Address,
        dividend_distributor: Address,
        fee_rate_bps: i64,
        min_order_size: i128,
        max_price_deviation_bps: i64,
    ) {
        if env.storage().instance().has(&Symbol::new(&env, "admin")) {
            panic_with_error!(&env, MarketError::AlreadyInitialized);
        }

        if fee_rate_bps < 0 || fee_rate_bps > 10000 {
            panic_with_error!(&env, MarketError::InvalidParameters);
        }

        if min_order_size <= 0 {
            panic_with_error!(&env, MarketError::InvalidParameters);
        }

        if max_price_deviation_bps < 0 || max_price_deviation_bps > 10000 {
            panic_with_error!(&env, MarketError::InvalidParameters);
        }

        let config = MarketConfig {
            admin: admin.clone(),
            fee_rate_bps,
            fee_recipient: admin.clone(),
            min_order_size,
            max_price_deviation_bps,
            is_paused: false,
            base_currency,
            compliance_registry,
            dividend_distributor,
        };

        crate::shared_admin::write_admin(&env, &admin, &admin);
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::OrderCount, &0u64);
        env.storage().instance().set(&DataKey::TradeCount, &0u64);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "version"), &STORAGE_VERSION);
    }

    fn read_version(env: &Env) -> u32 {
        env.storage()
            .instance()
            .get(&Symbol::new(env, "version"))
            .unwrap_or(0)
    }

    fn check_version(env: &Env) {
        if Self::read_version(env) < STORAGE_VERSION {
            panic_with_error!(env, MarketError::StorageOutdated);
        }
    }

    fn read_config(env: &Env) -> MarketConfig {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .unwrap_or_else(|| panic_with_error!(env, MarketError::MarketNotInitialized))
    }

    pub fn migrate(env: Env, auth: Address) {
        crate::shared_admin::require_admin(&env, &auth);

        let ver = Self::read_version(&env);
        if ver >= STORAGE_VERSION {
            panic_with_error!(&env, MarketError::AlreadyAtLatestVersion);
        }

        let mut current = ver;
        while current < STORAGE_VERSION {
            current += 1;
        }

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "version"), &STORAGE_VERSION);
    }

    pub fn update_config(env: Env, auth: Address, config: MarketConfig) {
        crate::shared_admin::require_admin(&env, &auth);
        env.storage().instance().set(&DataKey::Config, &config);
    }

    pub fn update_admin(env: Env, auth: Address, new_admin: Address) {
        crate::shared_admin::require_admin(&env, &auth);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "admin"), &new_admin);
    }

    pub fn place_order(
        env: Env,
        maker: Address,
        token_address: Address,
        side: Symbol,
        price: i128,
        amount: i128,
        expiry: u64,
        min_fill: i128,
    ) -> u64 {
        maker.require_auth();

        Self::check_version(&env);

        let config: MarketConfig = env.storage().instance().get(&DataKey::Config).unwrap();
        if config.is_paused {
            panic_with_error!(&env, MarketError::TradingPaused);
        }

        // 1. Compliance & Halt Checks
        Self::check_trading_halt(&env, &config, &token_address);
        Self::check_compliance(&env, &config, &maker, &token_address);

        // 2. Market Mechanic Checks
        if amount < config.min_order_size {
            panic_with_error!(&env, MarketError::MinOrderSizeNotMet);
        }
        if price <= 0 {
            panic_with_error!(&env, MarketError::InvalidOrder);
        }
        if expiry <= env.ledger().timestamp() {
            panic_with_error!(&env, MarketError::OrderExpired);
        }
        if min_fill < 0 || min_fill > amount {
            panic_with_error!(&env, MarketError::InvalidParameters);
        }

        // 3. Oracle price validation (issue #150): a fresh oracle price is the
        // reference, and a stale oracle price falls back to the VWAP reference.
        Self::validate_order_price(&env, &config, &token_address, price);

        // 4. Circuit breaker (issue #147): rejects orders while the breaker is
        // in its cooldown window.
        Self::enforce_circuit_breaker(&env, &token_address, Some(price));

        // 5. Escrow
        if side == Symbol::new(&env, "buy") {
            let total_cost = amount * price;
            let base_token = TokenClient::new(&env, &config.base_currency);
            base_token.transfer(&maker, &env.current_contract_address(), &total_cost);
        } else if side == Symbol::new(&env, "sell") {
            // Issue #145: explicitly verify the maker's balance before escrowing,
            // so a sell order with insufficient balance fails fast with a clear
            // error instead of surfacing a token-transfer error mid-escrow.
            let rwa_token = RWATokenClient::new(&env, &token_address);
            let maker_balance = rwa_token.get_balance(&maker).amount;
            if maker_balance < amount {
                panic_with_error!(&env, MarketError::InsufficientBalance);
            }
            rwa_token.transfer(&maker, &env.current_contract_address(), &amount);
        } else {
            panic_with_error!(&env, MarketError::InvalidOrder);
        }

        // 5. Create Order
        let mut count: u64 = env.storage().instance().get(&DataKey::OrderCount).unwrap();
        count += 1;
        env.storage().instance().set(&DataKey::OrderCount, &count);

        let order = Order {
            id: count,
            maker: maker.clone(),
            token_address: token_address.clone(),
            side: side.clone(),
            price,
            amount,
            filled_amount: 0,
            min_fill,
            expires_at: expiry,
            created_at: env.ledger().timestamp(),
            is_active: true,
        };

        // Store in Temporary storage
        env.storage()
            .temporary()
            .set(&DataKey::Order(count), &order);

        env.events().publish(
            (Symbol::new(&env, "order_placed"), token_address),
            (count, maker, side, price, amount, env.ledger().timestamp()),
        );

        // Issue #141: index the resting order in the price-time book.
        Self::book_insert(&env, &order);

        Self::auto_prune_expired_orders(&env);

        count
    }

    pub fn fill_order(env: Env, taker: Address, order_id: u64, fill_amount: i128) {
        taker.require_auth();

        Self::check_version(&env);

        Self::auto_prune_expired_orders(&env);

        let mut order: Order = env
            .storage()
            .temporary()
            .get(&DataKey::Order(order_id))
            .unwrap_or_else(|| {
                panic_with_error!(&env, MarketError::OrderNotFound);
            });

        if order.expires_at <= env.ledger().timestamp() {
            if order.is_active {
                env.events().publish(
                    (
                        Symbol::new(&env, "order_expired"),
                        order.token_address.clone(),
                    ),
                    (order_id, order.maker.clone(), env.ledger().timestamp()),
                );
            }
            panic_with_error!(&env, MarketError::OrderExpired);
        }

        if !order.is_active {
            panic_with_error!(&env, MarketError::OrderExpired);
        }

        let config: MarketConfig = env.storage().instance().get(&DataKey::Config).unwrap();
        if config.is_paused {
            panic_with_error!(&env, MarketError::TradingPaused);
        }

        // Issue #149: reject wash trades before any compliance or balance work.
        if taker == order.maker {
            panic_with_error!(&env, MarketError::SelfTrade);
        }

        // Issue #147: cooldown rejects every new order while tripped.
        Self::enforce_circuit_breaker(&env, &order.token_address, None);

        // Compliance & Halt Checks
        Self::check_trading_halt(&env, &config, &order.token_address);
        Self::check_compliance(&env, &config, &taker, &order.token_address);

        let remaining = order.amount - order.filled_amount;
        if fill_amount > remaining {
            panic_with_error!(&env, MarketError::InvalidOrder);
        }
        if fill_amount < order.min_fill && fill_amount < remaining {
            panic_with_error!(&env, MarketError::MinOrderSizeNotMet);
        }

        // Settlement & state update
        Self::settle_fill(&env, &taker, &mut order, fill_amount, &config);
    }

    /// Settle `fill_amount` against a resting `order` and persist the updated
    /// order (issue #140 refactor: shared by `fill_order` and
    /// `place_market_order`). Assumes all auth / limit / state checks have
    /// already been performed by the caller.
    fn settle_fill(
        env: &Env,
        taker: &Address,
        order: &mut Order,
        fill_amount: i128,
        config: &MarketConfig,
    ) {
        let order_id = order.id;

        // Issue #147: a trade far from the VWAP trips the breaker for the
        // cooldown window (the trade itself still settles).
        Self::trip_circuit_breaker_if_deviating(env, &order.token_address, order.price);

        let settlement_value = fill_amount * order.price;

        // Issue #148: apply the maker/taker fee tiers selected by each party's
        // 30-day volume. Until tiers are configured the legacy flat fee is kept
        // so existing markets behave exactly as before.
        let is_buy = order.side == Symbol::new(env, "buy");
        let (maker_fee, taker_fee) = if Self::has_fee_tiers(env) {
            let maker_bps = Self::fee_bps_for(env, &order.maker, false);
            let taker_bps = Self::fee_bps_for(env, taker, true);
            (
                (settlement_value * maker_bps as i128) / 10000,
                (settlement_value * taker_bps as i128) / 10000,
            )
        } else {
            let legacy_fee = (settlement_value * config.fee_rate_bps as i128) / 10000;
            (legacy_fee, 0)
        };
        let fee_total = maker_fee + taker_fee;
        let net_value = settlement_value - fee_total;

        let base_token = TokenClient::new(env, &config.base_currency);
        let rwa_token = RWATokenClient::new(env, &order.token_address);

        if is_buy {
            // Maker was buyer (escrowed payment), taker is seller.
            rwa_token.transfer(taker, &order.maker, &fill_amount);
            base_token.transfer(&env.current_contract_address(), taker, &net_value);
        } else {
            // Maker was seller (escrowed tokens), taker is buyer. The maker's
            // fee is deducted from their proceeds; the taker pays theirs on top.
            rwa_token.transfer(&env.current_contract_address(), taker, &fill_amount);
            base_token.transfer(taker, &order.maker, &(settlement_value - maker_fee));
        }

        if fee_total > 0 {
            if is_buy {
                base_token.transfer(
                    &env.current_contract_address(),
                    &config.fee_recipient,
                    &fee_total,
                );
            } else {
                base_token.transfer(taker, &config.fee_recipient, &fee_total);
            }
        }

        // Issue #148: 30-day rolling volume drives the next tier decision.
        Self::record_volume(env, taker, settlement_value);
        Self::record_volume(env, &order.maker, settlement_value);

        order.filled_amount += fill_amount;
        if order.filled_amount == order.amount {
            order.is_active = false;
            // Issue #141: a fully-filled order leaves the book.
            Self::book_remove(env, order);
        }
        env.storage()
            .temporary()
            .set(&DataKey::Order(order_id), order);

        Self::handle_trade_data(env, order.token_address.clone(), fill_amount, order.price);
        // Issue #142: evaluate conditional orders after every trade execution.
        Self::evaluate_conditional_orders(env.clone(), order.token_address.clone());

        env.events().publish(
            (Symbol::new(env, "trade"), order.token_address.clone()),
            (
                order_id,
                taker.clone(),
                fill_amount,
                order.price,
                env.ledger().timestamp(),
            ),
        );
    }

    /// Best active resting order on one side of the book, ranked by price then
    /// arrival time (issue #140 scan-based helper).
    fn best_opposing_order(
        env: &Env,
        token_address: &Address,
        resting_is_bid: bool,
        taker: &Address,
    ) -> Option<Order> {
        let count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::OrderCount)
            .unwrap_or(0);
        let now = env.ledger().timestamp();
        let want_side = if resting_is_bid {
            Symbol::new(env, "buy")
        } else {
            Symbol::new(env, "sell")
        };

        let mut best: Option<Order> = None;
        for id in 1..=count {
            if let Some(order) = env
                .storage()
                .temporary()
                .get::<DataKey, Order>(&DataKey::Order(id))
            {
                if !order.is_active
                    || order.expires_at <= now
                    || order.token_address != *token_address
                    || order.side != want_side
                    // Issue #149: never match an order against its own maker.
                    || order.maker == *taker
                {
                    continue;
                }

                let better = match &best {
                    None => true,
                    Some(b) => {
                        if order.price != b.price {
                            if resting_is_bid {
                                order.price > b.price
                            } else {
                                order.price < b.price
                            }
                        } else if order.created_at != b.created_at {
                            order.created_at < b.created_at
                        } else {
                            order.id < b.id
                        }
                    }
                };
                if better {
                    best = Some(order);
                }
            }
        }
        best
    }

    /// Execute a market order immediately against the best available resting
    /// orders (issue #140).
    ///
    /// A market buy fills the lowest-priced sells first; a market sell fills
    /// the highest-priced buys first. `limit_price` is the slippage bound: the
    /// worst price the taker is willing to accept (max for buys, min for
    /// sells). Partial fills are allowed and any unfilled remainder is simply
    /// left unexecuted — the taker never locks funds for a market order.
    ///
    /// Returns the total amount that was filled.
    pub fn place_market_order(
        env: Env,
        taker: Address,
        token_address: Address,
        side: Symbol,
        amount: i128,
        limit_price: i128,
    ) -> i128 {
        taker.require_auth();

        Self::check_version(&env);

        let config: MarketConfig = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .unwrap_or_else(|| panic_with_error!(&env, MarketError::MarketNotInitialized));
        if config.is_paused {
            panic_with_error!(&env, MarketError::TradingPaused);
        }

        Self::check_trading_halt(&env, &config, &token_address);
        Self::check_compliance(&env, &config, &taker, &token_address);

        let is_buy = side == Symbol::new(&env, "buy");
        let is_sell = side == Symbol::new(&env, "sell");
        if !is_buy && !is_sell {
            panic_with_error!(&env, MarketError::InvalidOrder);
        }
        if amount < config.min_order_size {
            panic_with_error!(&env, MarketError::MinOrderSizeNotMet);
        }
        if limit_price <= 0 {
            panic_with_error!(&env, MarketError::InvalidParameters);
        }

        // Issue #150 / #147: validate the market order's worst acceptable price
        // against the oracle (falling back to VWAP) and reject it while the
        // circuit breaker is in its cooldown window.
        Self::validate_order_price(&env, &config, &token_address, limit_price);
        Self::enforce_circuit_breaker(&env, &token_address, None);

        // The taker matches the opposite side of the book.
        let resting_is_bid = is_sell;

        env.events().publish(
            (
                Symbol::new(&env, "market_order_placed"),
                token_address.clone(),
            ),
            (
                taker.clone(),
                side.clone(),
                amount,
                limit_price,
                env.ledger().timestamp(),
            ),
        );

        // Slippage protection: reject when the best available price is already
        // outside the taker's limit.
        let best = match Self::best_opposing_order(&env, &token_address, resting_is_bid, &taker) {
            Some(order) => order,
            None => panic_with_error!(&env, MarketError::InsufficientLiquidity),
        };
        let best_ok = if is_buy {
            best.price <= limit_price
        } else {
            best.price >= limit_price
        };
        if !best_ok {
            panic_with_error!(&env, MarketError::SlippageExceeded);
        }

        let mut remaining = amount;
        let mut filled_total: i128 = 0;

        while remaining > 0 {
            let mut order =
                match Self::best_opposing_order(&env, &token_address, resting_is_bid, &taker) {
                    Some(order) => order,
                    None => break,
                };

            let within_limit = if is_buy {
                order.price <= limit_price
            } else {
                order.price >= limit_price
            };
            if !within_limit {
                break;
            }

            let available = order.amount - order.filled_amount;
            if available <= 0 {
                break;
            }
            let fill = if available < remaining {
                available
            } else {
                remaining
            };

            Self::settle_fill(&env, &taker, &mut order, fill, &config);

            env.events().publish(
                (
                    Symbol::new(&env, "market_order_filled"),
                    token_address.clone(),
                ),
                (order.id, fill, order.price, env.ledger().timestamp()),
            );

            remaining -= fill;
            filled_total += fill;
        }

        if filled_total == 0 {
            panic_with_error!(&env, MarketError::InsufficientLiquidity);
        }

        // Issue #142: evaluate conditional orders after every trade execution.
        Self::evaluate_conditional_orders(env.clone(), token_address.clone());

        filled_total
    }

    /// Execute a TWAP (time-weighted average price) order: split `total_amount`
    /// into `num_slices` equal slices and fill each slice at the best available
    /// price within `limit_price`. Slices execute sequentially in a single
    /// call; in production, an external scheduler would invoke the contract
    /// once per `interval_seconds` to advance the next slice. Returns the total
    /// amount filled (issue #144).
    pub fn place_twap_order(
        env: Env,
        taker: Address,
        token_address: Address,
        side: Symbol,
        total_amount: i128,
        num_slices: u32,
        _interval_seconds: u64,
        limit_price: i128,
    ) -> i128 {
        taker.require_auth();

        Self::check_version(&env);

        let config: MarketConfig = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .unwrap_or_else(|| panic_with_error!(&env, MarketError::MarketNotInitialized));
        if config.is_paused {
            panic_with_error!(&env, MarketError::TradingPaused);
        }

        let is_buy = side == Symbol::new(&env, "buy");
        let is_sell = side == Symbol::new(&env, "sell");
        if !is_buy && !is_sell {
            panic_with_error!(&env, MarketError::InvalidOrder);
        }
        if total_amount < config.min_order_size {
            panic_with_error!(&env, MarketError::MinOrderSizeNotMet);
        }
        if num_slices == 0 {
            panic_with_error!(&env, MarketError::InvalidParameters);
        }
        if limit_price <= 0 {
            panic_with_error!(&env, MarketError::InvalidParameters);
        }

        Self::check_trading_halt(&env, &config, &token_address);
        Self::check_compliance(&env, &config, &taker, &token_address);

        Self::validate_order_price(&env, &config, &token_address, limit_price);
        Self::enforce_circuit_breaker(&env, &token_address, None);

        let resting_is_bid = is_sell;
        let slice_size = total_amount / num_slices as i128;
        if slice_size == 0 {
            panic_with_error!(&env, MarketError::InvalidOrder);
        }

        let mut remaining = total_amount;
        let mut filled_total: i128 = 0;
        let mut slices_left = num_slices;

        while slices_left > 0 && remaining > 0 {
            let this_slice = if slice_size < remaining { slice_size } else { remaining };

            let best = match Self::best_opposing_order(&env, &token_address, resting_is_bid, &taker) {
                Some(order) => order,
                None => break,
            };

            let within_limit = if is_buy {
                best.price <= limit_price
            } else {
                best.price >= limit_price
            };
            if !within_limit {
                break;
            }

            let available = best.amount - best.filled_amount;
            if available <= 0 {
                break;
            }

            let fill = if available < this_slice { available } else { this_slice };

            Self::settle_fill(&env, &taker, &mut best.clone(), fill, &config);

            env.events().publish(
                (
                    Symbol::new(&env, "twap_order_filled"),
                    token_address.clone(),
                ),
                (fill, best.price, env.ledger().timestamp()),
            );

            remaining -= fill;
            filled_total += fill;
            slices_left -= 1;
        }

        if filled_total == 0 {
            panic_with_error!(&env, MarketError::InsufficientLiquidity);
        }

        Self::evaluate_conditional_orders(env.clone(), token_address.clone());

        filled_total
    }

    pub fn cancel_order(env: Env, maker: Address, order_id: u64) {
        maker.require_auth();

        Self::check_version(&env);

        let mut order: Order = env
            .storage()
            .temporary()
            .get(&DataKey::Order(order_id))
            .unwrap_or_else(|| {
                panic_with_error!(&env, MarketError::OrderNotFound);
            });

        if order.maker != maker {
            panic_with_error!(&env, MarketError::Unauthorized);
        }
        if !order.is_active {
            panic_with_error!(&env, MarketError::InvalidOrder);
        }

        let config: MarketConfig = env.storage().instance().get(&DataKey::Config).unwrap();
        let remaining = order.amount - order.filled_amount;

        // Refund Escrow
        if order.side == Symbol::new(&env, "buy") {
            let refund = remaining * order.price;
            let base_token = TokenClient::new(&env, &config.base_currency);
            base_token.transfer(&env.current_contract_address(), &maker, &refund);
        } else {
            let rwa_token = RWATokenClient::new(&env, &order.token_address);
            rwa_token.transfer(&env.current_contract_address(), &maker, &remaining);
        }

        order.is_active = false;
        env.storage()
            .temporary()
            .set(&DataKey::Order(order_id), &order);

        // Issue #141: drop the cancelled order from the book.
        Self::book_remove(&env, &order);

        env.events().publish(
            (Symbol::new(&env, "order_cancelled"), order.token_address),
            (
                order_id,
                maker,
                order.side,
                order.filled_amount,
                env.ledger().timestamp(),
            ),
        );
    }

    pub fn prune_expired_orders(env: Env) {
        Self::auto_prune_expired_orders(&env);
    }

    fn auto_prune_expired_orders(env: &Env) {
        let count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::OrderCount)
            .unwrap_or(0);
        if count == 0 {
            return;
        }
        let config: MarketConfig = match env.storage().instance().get(&DataKey::Config) {
            Some(c) => c,
            None => return,
        };
        let now = env.ledger().timestamp();
        for id in 1..=count {
            if let Some(mut order) = env
                .storage()
                .temporary()
                .get::<DataKey, Order>(&DataKey::Order(id))
            {
                if order.is_active && order.expires_at <= now {
                    let remaining = order.amount - order.filled_amount;
                    if remaining > 0 {
                        if order.side == Symbol::new(env, "buy") {
                            let refund = remaining * order.price;
                            let base_token = TokenClient::new(env, &config.base_currency);
                            base_token.transfer(
                                &env.current_contract_address(),
                                &order.maker,
                                &refund,
                            );
                        } else if order.side == Symbol::new(env, "sell") {
                            let rwa_token = RWATokenClient::new(env, &order.token_address);
                            rwa_token.transfer(
                                &env.current_contract_address(),
                                &order.maker,
                                &remaining,
                            );
                        }
                    }
                    order.is_active = false;
                    env.storage().temporary().set(&DataKey::Order(id), &order);
                    // Issue #141: pruned orders leave the book.
                    Self::book_remove(env, &order);
                    env.events().publish(
                        (
                            Symbol::new(env, "order_expired"),
                            order.token_address.clone(),
                        ),
                        (id, order.maker.clone(), now),
                    );
                }
            }
        }
    }

    // Helper: VWAP & TWAP Recording
    fn handle_trade_data(env: &Env, token: Address, amount: i128, price: i128) {
        let mut trade_count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::TradeCount)
            .unwrap_or(0);
        trade_count += 1;
        env.storage()
            .instance()
            .set(&DataKey::TradeCount, &trade_count);

        let now = env.ledger().timestamp();

        // Issue #142: remember the last traded price for trigger evaluation.
        env.storage()
            .persistent()
            .set(&DataKey::LastPrice(token.clone()), &price);

        // 1. Update VWAP (Volume Weighted Average Price)
        // Store as (total_volume, total_value)
        let vwap_key = DataKey::VWAP(token.clone());
        let (mut total_vol, mut total_val): (i128, i128) =
            env.storage().persistent().get(&vwap_key).unwrap_or((0, 0));

        total_vol += amount;
        total_val += amount * price;
        env.storage()
            .persistent()
            .set(&vwap_key, &(total_vol, total_val));

        // 2. Update TWAP (Time Weighted Average Price)
        // Store as (cumulative_price_time, last_price, last_time)
        let twap_key = DataKey::PriceHistory(token.clone());
        let (mut cum_price_time, last_price, last_time): (i128, i128, u64) = env
            .storage()
            .persistent()
            .get(&twap_key)
            .unwrap_or((0, price, now));

        let delta_t = (now - last_time) as i128;
        cum_price_time += last_price * delta_t;

        env.storage()
            .persistent()
            .set(&twap_key, &(cum_price_time, price, now));
    }

    pub fn get_vwap(env: Env, token: Address) -> i128 {
        let (total_vol, total_val): (i128, i128) = env
            .storage()
            .persistent()
            .get(&DataKey::VWAP(token))
            .unwrap_or((0, 0));
        if total_vol == 0 {
            return 0;
        }
        total_val / total_vol
    }

    pub fn get_twap(env: Env, token: Address, start_time: u64) -> i128 {
        let (cum_price_time, last_price, last_time): (i128, i128, u64) = env
            .storage()
            .persistent()
            .get(&DataKey::PriceHistory(token))
            .unwrap_or((0, 0, 0));

        let now = env.ledger().timestamp();
        if now == start_time {
            return last_price;
        }

        let total_cum = cum_price_time + (last_price * (now - last_time) as i128);
        // This is a simplified TWAP; a real one would need historical snapshots
        total_cum / (now - start_time) as i128
    }

    // ── Issue #142: stop-loss / take-profit conditional orders ────────────

    fn read_last_price(env: &Env, token: &Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::LastPrice(token.clone()))
            .unwrap_or(0)
    }

    fn user_conditional_key(owner: &Address, token: &Address) -> DataKey {
        DataKey::UserConditionalCount(owner.clone(), token.clone())
    }

    /// Place an off-book conditional (stop-loss / take-profit) order.
    ///
    /// Funds are escrowed immediately so the order can be converted into a
    /// resting limit order when it triggers without needing the owner's
    /// signature again. At most `MAX_CONDITIONAL_ORDERS_PER_USER` conditional
    /// orders may be live per user and token.
    pub fn place_conditional_order(
        env: Env,
        owner: Address,
        token_address: Address,
        side: Symbol,
        price: i128,
        amount: i128,
        condition: OrderCondition,
        expiry: u64,
    ) -> u64 {
        owner.require_auth();
        Self::check_version(&env);

        let config: MarketConfig = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .unwrap_or_else(|| panic_with_error!(&env, MarketError::MarketNotInitialized));
        if config.is_paused {
            panic_with_error!(&env, MarketError::TradingPaused);
        }

        Self::check_trading_halt(&env, &config, &token_address);
        Self::check_compliance(&env, &config, &owner, &token_address);

        if condition == OrderCondition::None {
            panic_with_error!(&env, MarketError::InvalidOrder);
        }
        if amount < config.min_order_size || price <= 0 {
            panic_with_error!(&env, MarketError::InvalidParameters);
        }
        if expiry <= env.ledger().timestamp() {
            panic_with_error!(&env, MarketError::OrderExpired);
        }

        let is_buy = side == Symbol::new(&env, "buy");
        let is_sell = side == Symbol::new(&env, "sell");
        if !is_buy && !is_sell {
            panic_with_error!(&env, MarketError::InvalidOrder);
        }

        // Per-user, per-token limit.
        let user_key = Self::user_conditional_key(&owner, &token_address);
        let live: u32 = env.storage().instance().get(&user_key).unwrap_or(0);
        if live >= MAX_CONDITIONAL_ORDERS_PER_USER {
            panic_with_error!(&env, MarketError::ConditionalOrderLimitReached);
        }

        // Escrow so the order can be activated later without the owner.
        if is_buy {
            let total_cost = amount * price;
            let base_token = TokenClient::new(&env, &config.base_currency);
            base_token.transfer(&owner, &env.current_contract_address(), &total_cost);
        } else {
            let rwa_token = RWATokenClient::new(&env, &token_address);
            rwa_token.transfer(&owner, &env.current_contract_address(), &amount);
        }

        let mut count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::ConditionalCount)
            .unwrap_or(0);
        count += 1;
        env.storage()
            .instance()
            .set(&DataKey::ConditionalCount, &count);

        let cond = ConditionalOrder {
            id: count,
            owner: owner.clone(),
            token_address: token_address.clone(),
            side: side.clone(),
            price,
            amount,
            filled_amount: 0,
            condition,
            expires_at: expiry,
            created_at: env.ledger().timestamp(),
            is_active: true,
            activated_order_id: 0,
        };
        env.storage()
            .instance()
            .set(&DataKey::ConditionalOrder(count), &cond);
        env.storage().instance().set(&user_key, &(live + 1));

        env.events().publish(
            (Symbol::new(&env, "conditional_order_placed"), token_address),
            (count, owner, side, price, amount, env.ledger().timestamp()),
        );

        count
    }

    /// Read a conditional order by id.
    pub fn get_conditional_order(env: Env, id: u64) -> ConditionalOrder {
        env.storage()
            .instance()
            .get(&DataKey::ConditionalOrder(id))
            .unwrap_or_else(|| panic_with_error!(&env, MarketError::ConditionalOrderNotFound))
    }

    /// Cancel a live conditional order and refund its escrow to the owner.
    pub fn cancel_conditional_order(env: Env, owner: Address, id: u64) {
        owner.require_auth();
        Self::check_version(&env);

        let mut cond: ConditionalOrder = env
            .storage()
            .instance()
            .get(&DataKey::ConditionalOrder(id))
            .unwrap_or_else(|| panic_with_error!(&env, MarketError::ConditionalOrderNotFound));

        if cond.owner != owner {
            panic_with_error!(&env, MarketError::Unauthorized);
        }
        if !cond.is_active {
            panic_with_error!(&env, MarketError::InvalidOrder);
        }

        let config = Self::read_config(&env);
        Self::refund_conditional(&env, &config, &cond);
        cond.is_active = false;

        env.storage()
            .instance()
            .set(&DataKey::ConditionalOrder(id), &cond);
        Self::decrement_user_conditional(&env, &cond.owner, &cond.token_address);
    }

    /// Evaluate every live conditional order for `token_address` against the
    /// last traded price. Called automatically after each trade.
    pub fn evaluate_conditional_orders(env: Env, token_address: Address) {
        let count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::ConditionalCount)
            .unwrap_or(0);
        if count == 0 {
            return;
        }

        let last_price = Self::read_last_price(&env, &token_address);
        let now = env.ledger().timestamp();
        let config = Self::read_config(&env);

        for id in 1..=count {
            let mut cond: ConditionalOrder =
                match env.storage().instance().get(&DataKey::ConditionalOrder(id)) {
                    Some(c) => c,
                    None => continue,
                };

            if !cond.is_active || cond.token_address != token_address {
                continue;
            }

            // Expiry is enforced independently of the traded price.
            if cond.expires_at <= now {
                cond.is_active = false;
                Self::refund_conditional(&env, &config, &cond);
                env.storage()
                    .instance()
                    .set(&DataKey::ConditionalOrder(id), &cond);
                Self::decrement_user_conditional(&env, &cond.owner, &cond.token_address);
                env.events().publish(
                    (
                        Symbol::new(&env, "conditional_order_expired"),
                        cond.token_address.clone(),
                    ),
                    (id, cond.owner.clone(), now),
                );
                continue;
            }

            if last_price == 0 {
                continue;
            }

            let triggered = match &cond.condition {
                OrderCondition::StopLoss(trigger) => last_price <= *trigger,
                OrderCondition::TakeProfit(trigger) => last_price >= *trigger,
                OrderCondition::None => false,
            };
            if !triggered {
                continue;
            }

            // Convert into a resting order (funds are already escrowed).
            cond.activated_order_id = Self::activate_conditional_order(&env, &cond);
            cond.is_active = false;
            env.storage()
                .instance()
                .set(&DataKey::ConditionalOrder(id), &cond);
            Self::decrement_user_conditional(&env, &cond.owner, &cond.token_address);
        }
    }

    fn refund_conditional(env: &Env, config: &MarketConfig, cond: &ConditionalOrder) {
        let remaining = cond.amount - cond.filled_amount;
        if remaining <= 0 {
            return;
        }
        if cond.side == Symbol::new(env, "buy") {
            let refund = remaining * cond.price;
            let base_token = TokenClient::new(env, &config.base_currency);
            base_token.transfer(&env.current_contract_address(), &cond.owner, &refund);
        } else {
            let rwa_token = RWATokenClient::new(env, &cond.token_address);
            rwa_token.transfer(&env.current_contract_address(), &cond.owner, &remaining);
        }
    }

    fn activate_conditional_order(env: &Env, cond: &ConditionalOrder) -> u64 {
        let mut count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::OrderCount)
            .unwrap_or(0);
        count += 1;
        env.storage().instance().set(&DataKey::OrderCount, &count);

        let order = Order {
            id: count,
            maker: cond.owner.clone(),
            token_address: cond.token_address.clone(),
            side: cond.side.clone(),
            price: cond.price,
            amount: cond.amount,
            filled_amount: cond.filled_amount,
            min_fill: 0,
            expires_at: cond.expires_at,
            created_at: env.ledger().timestamp(),
            is_active: true,
        };
        env.storage()
            .temporary()
            .set(&DataKey::Order(count), &order);

        // Issue #141: activated conditional orders rest on the book.
        Self::book_insert(env, &order);

        env.events().publish(
            (
                Symbol::new(env, "conditional_order_triggered"),
                cond.token_address.clone(),
            ),
            (cond.id, count, cond.price, env.ledger().timestamp()),
        );
        env.events().publish(
            (Symbol::new(env, "order_placed"), cond.token_address.clone()),
            (
                count,
                cond.owner.clone(),
                cond.side.clone(),
                cond.price,
                cond.amount,
                env.ledger().timestamp(),
            ),
        );

        count
    }

    fn decrement_user_conditional(env: &Env, owner: &Address, token: &Address) {
        let key = Self::user_conditional_key(owner, token);
        let live: u32 = env.storage().instance().get(&key).unwrap_or(0);
        if live > 0 {
            env.storage().instance().set(&key, &(live - 1));
        }
    }

    // ── Issue #141: price-time-priority order book ────────────────────────
    //
    // Each side is stored as a sorted list of price levels plus a map from a
    // price level to its resting order ids in arrival (time) order. The best
    // price is therefore always at index 0 (O(1) best-price lookup) and a
    // level is located in O(log n) with a binary search.

    fn read_prices(env: &Env, token: &Address, is_bid: bool) -> Vec<i128> {
        let key = if is_bid {
            DataKey::BidPrices(token.clone())
        } else {
            DataKey::AskPrices(token.clone())
        };
        env.storage()
            .persistent()
            .get(&key)
            .unwrap_or(Vec::new(env))
    }

    fn write_prices(env: &Env, token: &Address, is_bid: bool, prices: &Vec<i128>) {
        let key = if is_bid {
            DataKey::BidPrices(token.clone())
        } else {
            DataKey::AskPrices(token.clone())
        };
        env.storage().persistent().set(&key, prices);
    }

    fn orders_key(token: &Address, is_bid: bool, price: i128) -> DataKey {
        if is_bid {
            DataKey::BidOrders(token.clone(), price)
        } else {
            DataKey::AskOrders(token.clone(), price)
        }
    }

    /// Insert a resting order, keeping strict price-time priority.
    fn book_insert(env: &Env, order: &Order) {
        let is_bid = order.side == Symbol::new(env, "buy");
        let token = order.token_address.clone();
        let mut prices = Self::read_prices(env, &token, is_bid);

        // Binary search for the price level (desc for bids, asc for asks).
        let mut lo: u32 = 0;
        let mut hi: u32 = prices.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let p = prices.get(mid).unwrap();
            let move_right = if is_bid {
                order.price < p
            } else {
                order.price > p
            };
            if move_right {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }

        let price_present = lo < prices.len() && prices.get(lo).unwrap() == order.price;
        if !price_present {
            prices.insert(lo, order.price);
            Self::write_prices(env, &token, is_bid, &prices);
        }

        // Append at the level: arrival order is time priority.
        let level_key = Self::orders_key(&token, is_bid, order.price);
        let mut ids: Vec<u64> = env
            .storage()
            .persistent()
            .get(&level_key)
            .unwrap_or(Vec::new(env));
        ids.push_back(order.id);
        env.storage().persistent().set(&level_key, &ids);
    }

    /// Remove an order from the book (cancel, full fill or expiry).
    fn book_remove(env: &Env, order: &Order) {
        let is_bid = order.side == Symbol::new(env, "buy");
        let token = order.token_address.clone();
        let level_key = Self::orders_key(&token, is_bid, order.price);

        let ids: Vec<u64> = env
            .storage()
            .persistent()
            .get(&level_key)
            .unwrap_or(Vec::new(env));

        let mut kept = Vec::<u64>::new(env);
        let mut removed = false;
        for id in ids.iter() {
            if id == order.id {
                removed = true;
            } else {
                kept.push_back(id);
            }
        }

        if !removed {
            return;
        }

        if kept.len() == 0 {
            env.storage().persistent().remove(&level_key);
            // Drop the now-empty price level.
            let prices = Self::read_prices(env, &token, is_bid);
            let mut new_prices = Vec::<i128>::new(env);
            for p in prices.iter() {
                if p != order.price {
                    new_prices.push_back(p);
                }
            }
            Self::write_prices(env, &token, is_bid, &new_prices);
        } else {
            env.storage().persistent().set(&level_key, &kept);
        }
    }

    fn book_levels(env: &Env, token: &Address, is_bid: bool) -> Vec<OrderBookLevel> {
        let prices = Self::read_prices(env, token, is_bid);
        let mut levels = Vec::<OrderBookLevel>::new(env);
        for price in prices.iter() {
            let key = Self::orders_key(token, is_bid, price);
            let ids: Vec<u64> = env
                .storage()
                .persistent()
                .get(&key)
                .unwrap_or(Vec::new(env));
            let mut total: i128 = 0;
            let mut count: u32 = 0;
            for id in ids.iter() {
                if let Some(order) = env
                    .storage()
                    .temporary()
                    .get::<DataKey, Order>(&DataKey::Order(id))
                {
                    if order.is_active {
                        total += order.amount - order.filled_amount;
                        count += 1;
                    }
                }
            }
            if count > 0 {
                levels.push_back(OrderBookLevel {
                    price,
                    total_amount: total,
                    order_count: count,
                });
            }
        }
        levels
    }

    fn best_order_id(env: &Env, token: &Address, is_bid: bool) -> Option<u64> {
        let prices = Self::read_prices(env, token, is_bid);
        let now = env.ledger().timestamp();
        for price in prices.iter() {
            let key = Self::orders_key(token, is_bid, price);
            let ids: Vec<u64> = env
                .storage()
                .persistent()
                .get(&key)
                .unwrap_or(Vec::new(env));
            for id in ids.iter() {
                if let Some(order) = env
                    .storage()
                    .temporary()
                    .get::<DataKey, Order>(&DataKey::Order(id))
                {
                    if order.is_active && order.expires_at > now {
                        return Some(id);
                    }
                }
            }
        }
        None
    }

    fn best_order(env: &Env, token: &Address, is_bid: bool) -> Option<BookOrder> {
        let id = Self::best_order_id(env, token, is_bid)?;
        let order: Order = env.storage().temporary().get(&DataKey::Order(id))?;
        Some(BookOrder {
            order_id: id,
            maker: order.maker.clone(),
            side: order.side.clone(),
            price: order.price,
            remaining: order.amount - order.filled_amount,
            created_at: order.created_at,
        })
    }

    /// Full aggregated order book for `token`, sorted by price-time priority.
    pub fn get_order_book(env: Env, token: Address) -> OrderBook {
        OrderBook {
            bids: Self::book_levels(&env, &token, true),
            asks: Self::book_levels(&env, &token, false),
        }
    }

    /// Aggregated order-book depth for `token` on `side` within `price_range_bps`
    /// basis points of the best price. For bids, depth accumulates downward from
    /// the best bid; for asks, upward from the best ask (issue #146).
    pub fn get_order_book_depth(
        env: Env,
        token: Address,
        side: Symbol,
        price_range_bps: u64,
    ) -> OrderBookDepth {
        let is_bid = side == Symbol::new(&env, "buy");
        let is_ask = side == Symbol::new(&env, "sell");
        if !is_bid && !is_ask {
            panic_with_error!(&env, MarketError::InvalidOrder);
        }

        let book = Self::get_order_book(env.clone(), token.clone());
        let levels = if is_bid {
            book.bids
        } else {
            book.asks
        };

        let best_price = match levels.get(0) {
            Some(l) => l.price,
            None => return OrderBookDepth {
                best_price: 0,
                total_amount: 0,
                order_count: 0,
                levels: Vec::new(&env),
            },
        };

        let mut included = Vec::<OrderBookLevel>::new(&env);
        let mut total_amount: i128 = 0;
        let mut order_count: u32 = 0;

        for level in levels.iter() {
            let in_range = if is_bid {
                // Bids: include prices >= best_price * (1 - range_bps / 10000)
                let min_price = best_price
                    .saturating_sub((best_price * price_range_bps as i128) / 10000i128);
                level.price >= min_price
            } else {
                // Asks: include prices <= best_price * (1 + range_bps / 10000)
                let max_price = best_price
                    .saturating_add((best_price * price_range_bps as i128) / 10000i128);
                level.price <= max_price
            };
            if in_range {
                included.push_back(level.clone());
                total_amount += level.total_amount;
                order_count += level.order_count;
            } else {
                break;
            }
        }

        OrderBookDepth {
            best_price,
            total_amount,
            order_count,
            levels: included,
        }
    }

    /// Best (highest-priced) resting buy order.
    pub fn get_best_bid(env: Env, token: Address) -> Option<BookOrder> {
        Self::best_order(&env, &token, true)
    }

    /// Best (lowest-priced) resting sell order.
    pub fn get_best_ask(env: Env, token: Address) -> Option<BookOrder> {
        Self::best_order(&env, &token, false)
    }

    /// Match an incoming `taker_side` order against the best-priced resting
    /// order on the opposite side and fill it completely.
    ///
    /// Returns the id of the order that was filled. Panics with
    /// `InsufficientLiquidity` when the opposite side of the book is empty.
    pub fn match_best_order(
        env: Env,
        taker: Address,
        token_address: Address,
        taker_side: Symbol,
    ) -> u64 {
        let is_bid_taker = taker_side == Symbol::new(&env, "buy");
        // A buyer matches resting sells (asks); a seller matches resting buys.
        let resting_is_bid = !is_bid_taker;
        let best = Self::best_order_id(&env, &token_address, resting_is_bid)
            .unwrap_or_else(|| panic_with_error!(&env, MarketError::InsufficientLiquidity));

        let order: Order = env
            .storage()
            .temporary()
            .get(&DataKey::Order(best))
            .unwrap_or_else(|| panic_with_error!(&env, MarketError::OrderNotFound));
        let remaining = order.amount - order.filled_amount;

        // `fill_order` performs the single authorisation and settlement.
        Self::fill_order(env, taker, best, remaining);
        best
    }
    // --- Private Logics ---

    fn check_trading_halt(env: &Env, config: &MarketConfig, token: &Address) {
        // Halt during dividend record dates
        // This requires the DividendDistributor to provide a `is_record_date_active(token)` method
        // For now, placeholder check
        let distributor = DividendDistributorClient::new(env, &config.dividend_distributor);
        // if distributor.is_record_date_active(token) { panic!("Trading halted: Dividend record date"); }
    }

    fn check_compliance(env: &Env, config: &MarketConfig, user: &Address, token: &Address) {
        let registry = ComplianceRegistryClient::new(env, &config.compliance_registry);
        // The registry owns the KYC policy: when KYC is not required every
        // participant passes, otherwise the participant must be verified.
        if registry.is_kyc_required() {
            let kyc = registry.get_kyc_status(user);
            if !kyc.is_verified {
                panic_with_error!(env, MarketError::ComplianceFailed);
            }
        }
    }

    // ── Issue #147: circuit breaker ───────────────────────────────────────

    /// Configure the circuit breaker (admin only).
    ///
    /// Once the market has a configured breaker, a price that deviates from
    /// the VWAP by more than `threshold_bps` trips it. While tripped every new
    /// order is rejected until the cooldown elapses (and, when `auto_reset` is
    /// set, the market has stabilised) or an admin force-resets it.
    pub fn set_circuit_breaker_config(env: Env, auth: Address, config: CircuitBreakerConfig) {
        crate::shared_admin::require_admin(&env, &auth);
        if config.threshold_bps < 0 || config.threshold_bps > 10000 {
            panic_with_error!(&env, MarketError::InvalidParameters);
        }
        env.storage()
            .instance()
            .set(&DataKey::CircuitBreakerConfig, &config);
        env.events().publish(
            (Symbol::new(&env, "circuit_breaker_config_updated"),),
            (
                config.threshold_bps,
                config.cooldown_seconds,
                config.auto_reset,
                env.ledger().timestamp(),
            ),
        );
    }

    /// Circuit breaker parameters, defaulting to the market's configured
    /// `max_price_deviation_bps` with no cooldown and auto-reset enabled.
    pub fn get_circuit_breaker_config(env: Env) -> CircuitBreakerConfig {
        Self::read_circuit_breaker_config(&env)
    }

    /// Current breaker state for `token`.
    pub fn get_circuit_breaker_state(env: Env, token: Address) -> CircuitBreakerState {
        Self::read_circuit_breaker_state(&env, &token)
    }

    pub fn is_circuit_breaker_tripped(env: Env, token: Address) -> bool {
        Self::read_circuit_breaker_state(&env, &token).is_tripped
    }

    /// Manual override: force a tripped breaker back to normal (admin only).
    pub fn force_reset_circuit_breaker(env: Env, auth: Address, token: Address) {
        crate::shared_admin::require_admin(&env, &auth);
        let state = CircuitBreakerState {
            is_tripped: false,
            tripped_at: 0,
            reference_price: 0,
            trip_price: 0,
        };
        Self::write_circuit_breaker_state(&env, &token, &state);
        env.events().publish(
            (Symbol::new(&env, "circuit_breaker_force_reset"), token),
            (auth, env.ledger().timestamp()),
        );
    }

    fn read_circuit_breaker_config(env: &Env) -> CircuitBreakerConfig {
        env.storage()
            .instance()
            .get(&DataKey::CircuitBreakerConfig)
            .unwrap_or_else(|| {
                let market = Self::read_config(env);
                CircuitBreakerConfig {
                    threshold_bps: market.max_price_deviation_bps,
                    cooldown_seconds: 0,
                    auto_reset: true,
                }
            })
    }

    fn read_circuit_breaker_state(env: &Env, token: &Address) -> CircuitBreakerState {
        env.storage()
            .persistent()
            .get(&DataKey::CircuitBreakerState(token.clone()))
            .unwrap_or(CircuitBreakerState {
                is_tripped: false,
                tripped_at: 0,
                reference_price: 0,
                trip_price: 0,
            })
    }

    fn write_circuit_breaker_state(env: &Env, token: &Address, state: &CircuitBreakerState) {
        env.storage()
            .persistent()
            .set(&DataKey::CircuitBreakerState(token.clone()), state);
    }

    /// Reject new activity while the breaker is tripped. With `auto_reset` on,
    /// the breaker clears itself once the cooldown has elapsed and the trip
    /// price is no longer deviating from the reference price.
    fn enforce_circuit_breaker(env: &Env, token: &Address, incoming_price: Option<i128>) {
        let state = Self::read_circuit_breaker_state(env, token);
        if !state.is_tripped {
            return;
        }

        let config = Self::read_circuit_breaker_config(env);
        let now = env.ledger().timestamp();
        if config.auto_reset && now >= state.tripped_at + config.cooldown_seconds {
            let vwap = Self::get_vwap(env.clone(), token.clone());
            let reference = if vwap > 0 {
                vwap
            } else {
                state.reference_price
            };
            // Stabilisation is judged on the price being submitted, falling back
            // to the last traded price for orders that carry no price.
            let current = match incoming_price {
                Some(price) if price > 0 => price,
                _ => {
                    let last = Self::read_last_price(env, token);
                    if last > 0 {
                        last
                    } else {
                        state.trip_price
                    }
                }
            };
            if reference <= 0
                || price_deviation_bps(current, reference) <= config.threshold_bps as i128
            {
                let cleared = CircuitBreakerState {
                    is_tripped: false,
                    tripped_at: 0,
                    reference_price: reference,
                    trip_price: 0,
                };
                Self::write_circuit_breaker_state(env, token, &cleared);
                env.events().publish(
                    (Symbol::new(env, "circuit_breaker_reset"), token.clone()),
                    (now,),
                );
                return;
            }
        }

        panic_with_error!(env, MarketError::CircuitBreakerTripped);
    }

    /// Trip the breaker when an executed trade deviates too far from the VWAP.
    ///
    /// The breaker is opt-in: nothing happens until an admin configures one via
    /// `set_circuit_breaker_config`. The trade that trips it still settles (a
    /// Soroban panic would roll back the trip itself); every order submitted
    /// afterwards is then rejected for the cooldown window.
    fn trip_circuit_breaker_if_deviating(env: &Env, token: &Address, trade_price: i128) {
        if !env.storage().instance().has(&DataKey::CircuitBreakerConfig) {
            return;
        }
        let state = Self::read_circuit_breaker_state(env, token);
        if state.is_tripped {
            return;
        }

        let vwap = Self::get_vwap(env.clone(), token.clone());
        if vwap <= 0 {
            return;
        }

        let config = Self::read_circuit_breaker_config(env);
        let deviation = price_deviation_bps(trade_price, vwap);
        if deviation > config.threshold_bps as i128 {
            let tripped = CircuitBreakerState {
                is_tripped: true,
                tripped_at: env.ledger().timestamp(),
                reference_price: vwap,
                trip_price: trade_price,
            };
            Self::write_circuit_breaker_state(env, token, &tripped);
            env.events().publish(
                (Symbol::new(env, "circuit_breaker_tripped"), token.clone()),
                (trade_price, vwap, deviation, env.ledger().timestamp()),
            );
        }
    }

    // ── Issue #148: volume-based fee tiers ────────────────────────────────

    /// Fee tiers described in issue #148: <100k = 30bps, 100k-1M = 20bps,
    /// 1M-10M = 15bps, >10M = 10bps.
    fn default_fee_tiers(env: &Env) -> Vec<FeeTier> {
        Vec::from_array(
            env,
            [
                FeeTier {
                    min_30d_volume: 0,
                    maker_fee_bps: 30,
                    taker_fee_bps: 30,
                },
                FeeTier {
                    min_30d_volume: 100_000,
                    maker_fee_bps: 20,
                    taker_fee_bps: 20,
                },
                FeeTier {
                    min_30d_volume: 1_000_000,
                    maker_fee_bps: 15,
                    taker_fee_bps: 15,
                },
                FeeTier {
                    min_30d_volume: 10_000_000,
                    maker_fee_bps: 10,
                    taker_fee_bps: 10,
                },
            ],
        )
    }

    /// Replace the fee-tier ladder (admin only). Tiers must be ordered by
    /// strictly ascending `min_30d_volume`.
    pub fn set_fee_tiers(env: Env, auth: Address, tiers: Vec<FeeTier>) {
        crate::shared_admin::require_admin(&env, &auth);
        if tiers.len() == 0 {
            panic_with_error!(&env, MarketError::InvalidParameters);
        }

        let mut previous: i128 = -1;
        for tier in tiers.iter() {
            if tier.min_30d_volume < 0
                || tier.min_30d_volume <= previous
                || tier.maker_fee_bps < 0
                || tier.maker_fee_bps > 10000
                || tier.taker_fee_bps < 0
                || tier.taker_fee_bps > 10000
            {
                panic_with_error!(&env, MarketError::InvalidParameters);
            }
            previous = tier.min_30d_volume;
        }

        env.storage().instance().set(&DataKey::FeeTiers, &tiers);
        env.events().publish(
            (Symbol::new(&env, "fee_tiers_updated"),),
            (tiers.len(), env.ledger().timestamp()),
        );
    }

    /// Configured fee-tier ladder, or the issue #148 defaults when unset.
    pub fn get_fee_tiers(env: Env) -> Vec<FeeTier> {
        Self::read_fee_tiers(&env)
    }

    fn read_fee_tiers(env: &Env) -> Vec<FeeTier> {
        env.storage()
            .instance()
            .get(&DataKey::FeeTiers)
            .unwrap_or_else(|| Self::default_fee_tiers(env))
    }

    /// True once an explicit ladder has been stored, i.e. tiered fees are live.
    fn has_fee_tiers(env: &Env) -> bool {
        env.storage().instance().has(&DataKey::FeeTiers)
    }

    /// Trailing 30-day traded volume for `trader`, in base-currency units.
    pub fn get_30d_volume(env: Env, trader: Address) -> i128 {
        Self::read_30d_volume(&env, &trader)
    }

    fn read_30d_volume(env: &Env, trader: &Address) -> i128 {
        let history = Self::prune_volume_history(env, trader);
        let mut total: i128 = 0;
        for entry in history.iter() {
            total += entry.1;
        }
        total
    }

    fn prune_volume_history(env: &Env, trader: &Address) -> Vec<(u64, i128)> {
        let cutoff = env
            .ledger()
            .timestamp()
            .saturating_sub(VOLUME_WINDOW_SECONDS);
        let stored: Vec<(u64, i128)> = env
            .storage()
            .persistent()
            .get(&DataKey::VolumeHistory(trader.clone()))
            .unwrap_or(Vec::new(env));
        let mut kept: Vec<(u64, i128)> = Vec::new(env);
        for entry in stored.iter() {
            if entry.0 >= cutoff {
                kept.push_back(entry);
            }
        }
        kept
    }

    /// Record settled volume and emit `trader_tier_changed` when the trader
    /// moves between fee tiers.
    fn record_volume(env: &Env, trader: &Address, volume: i128) {
        if volume <= 0 {
            return;
        }

        let tier_before = Self::fee_bps_for(env, trader, false);

        let mut history = Self::prune_volume_history(env, trader);
        history.push_back((env.ledger().timestamp(), volume));
        env.storage()
            .persistent()
            .set(&DataKey::VolumeHistory(trader.clone()), &history);

        let volume_30d = Self::read_30d_volume(env, trader);
        let tier_after = Self::fee_bps_for(env, trader, false);
        env.storage()
            .persistent()
            .set(&DataKey::TraderFeeBps(trader.clone()), &tier_after);

        if tier_before != tier_after {
            env.events().publish(
                (Symbol::new(env, "trader_tier_changed"), trader.clone()),
                (
                    tier_before,
                    tier_after,
                    volume_30d,
                    env.ledger().timestamp(),
                ),
            );
        }
    }

    /// Fee tier that applies to `trader` for their trailing 30-day volume.
    pub fn get_fee_tier(env: Env, trader: Address) -> FeeTier {
        let volume = Self::read_30d_volume(&env, &trader);
        Self::tier_for_volume(&env, volume)
    }

    fn tier_for_volume(env: &Env, volume: i128) -> FeeTier {
        let tiers = Self::read_fee_tiers(env);
        let mut selected = tiers.get(0).unwrap();
        for tier in tiers.iter() {
            if volume >= tier.min_30d_volume {
                selected = tier;
            }
        }
        selected
    }

    /// Maker or taker fee (bps) for `trader`, resolved from their tier.
    fn fee_bps_for(env: &Env, trader: &Address, is_taker: bool) -> i64 {
        let volume = Self::read_30d_volume(env, trader);
        let tier = Self::tier_for_volume(env, volume);
        if is_taker {
            tier.taker_fee_bps
        } else {
            tier.maker_fee_bps
        }
    }

    // ── Issue #150: external price oracle ─────────────────────────────────

    /// Register the oracle allowed to publish prices for `token` (admin only).
    pub fn set_price_oracle(env: Env, auth: Address, token: Address, oracle_address: Address) {
        crate::shared_admin::require_admin(&env, &auth);
        env.storage()
            .persistent()
            .set(&DataKey::PriceOracle(token.clone()), &oracle_address);
        env.events().publish(
            (Symbol::new(&env, "price_oracle_set"), token),
            (oracle_address, env.ledger().timestamp()),
        );
    }

    /// Remove the oracle configuration (and its last price) for `token`.
    pub fn remove_price_oracle(env: Env, auth: Address, token: Address) {
        crate::shared_admin::require_admin(&env, &auth);
        env.storage()
            .persistent()
            .remove(&DataKey::PriceOracle(token.clone()));
        env.storage()
            .persistent()
            .remove(&DataKey::OraclePrice(token));
    }

    /// Oracle currently configured for `token`, if any.
    pub fn get_configured_oracle(env: Env, token: Address) -> Option<Address> {
        env.storage().persistent().get(&DataKey::PriceOracle(token))
    }

    /// Publish a price for `token`. Only the configured oracle may do so.
    pub fn set_oracle_price(
        env: Env,
        oracle: Address,
        token: Address,
        price: i128,
        confidence_interval: i128,
    ) {
        oracle.require_auth();

        let configured: Address = env
            .storage()
            .persistent()
            .get(&DataKey::PriceOracle(token.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, MarketError::Unauthorized));
        if configured != oracle {
            panic_with_error!(&env, MarketError::Unauthorized);
        }
        if price <= 0 || confidence_interval < 0 {
            panic_with_error!(&env, MarketError::InvalidParameters);
        }

        let entry = OraclePrice {
            token: token.clone(),
            price,
            timestamp: env.ledger().timestamp(),
            oracle_address: oracle,
            confidence_interval,
        };
        env.storage()
            .persistent()
            .set(&DataKey::OraclePrice(token.clone()), &entry);
        env.events().publish(
            (Symbol::new(&env, "oracle_price_updated"), token),
            (price, confidence_interval, env.ledger().timestamp()),
        );
    }

    /// Latest oracle price recorded for `token`, if any.
    pub fn get_oracle_price(env: Env, token: Address) -> Option<OraclePrice> {
        env.storage().persistent().get(&DataKey::OraclePrice(token))
    }

    /// Stored oracle price when it is still fresh (published within the last
    /// hour), otherwise `None` so callers fall back to VWAP.
    fn read_fresh_oracle_price(env: &Env, token: &Address) -> Option<OraclePrice> {
        let entry: OraclePrice = env
            .storage()
            .persistent()
            .get(&DataKey::OraclePrice(token.clone()))?;
        if is_oracle_price_fresh(
            entry.timestamp,
            env.ledger().timestamp(),
            ORACLE_STALE_SECONDS,
        ) {
            Some(entry)
        } else {
            None
        }
    }

    /// Validate an order price against the configured oracle price, falling
    /// back to the market's own VWAP reference when the oracle price is stale
    /// (older than an hour).
    ///
    /// Orders on a token with **no** oracle configured keep the legacy
    /// behaviour: a far-out limit price is simply a resting order that will not
    /// fill, and price protection is provided by the opt-in circuit breaker.
    fn validate_order_price(env: &Env, config: &MarketConfig, token: &Address, price: i128) {
        let max_deviation = config.max_price_deviation_bps as i128;

        if let Some(oracle) = Self::read_fresh_oracle_price(env, token) {
            let deviation = price_deviation_bps(price, oracle.price);
            if deviation > max_deviation {
                env.events().publish(
                    (Symbol::new(env, "oracle_price_deviation"), token.clone()),
                    (price, oracle.price, deviation),
                );
                panic_with_error!(env, MarketError::OraclePriceDeviation);
            }
            return;
        }

        // An oracle exists but its price is stale: fall back to VWAP.
        if !env
            .storage()
            .persistent()
            .has(&DataKey::OraclePrice(token.clone()))
        {
            return;
        }

        let vwap = Self::get_vwap(env.clone(), token.clone());
        if vwap > 0 {
            let deviation = price_deviation_bps(price, vwap);
            if deviation > max_deviation {
                env.events().publish(
                    (Symbol::new(env, "oracle_price_deviation"), token.clone()),
                    (price, vwap, deviation),
                );
                panic_with_error!(env, MarketError::OraclePriceDeviation);
            }
        }
    }
}
