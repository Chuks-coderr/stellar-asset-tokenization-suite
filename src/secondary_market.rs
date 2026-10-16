use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, token::TokenClient, Address, Env, Map,
    Symbol, Vec, log,
};

use crate::rwa_token::RWATokenClient;
use crate::dividend_distributor::DividendDistributorClient;
use crate::compliance_registry::ComplianceRegistryClient;

const STORAGE_VERSION: u32 = 1;

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
    // Issue #141: sorted order-book indexes.
    BidPrices(Address),          // Vec<i128>, descending
    AskPrices(Address),          // Vec<i128>, ascending
    BidOrders(Address, i128),    // Vec<u64> order ids at a bid price level
    AskOrders(Address, i128),    // Vec<u64> order ids at an ask price level
}

// ... (around line 80)
#[contract]
pub struct SecondaryMarket;

#[contractimpl]
impl SecondaryMarket {
// ...

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

        // 3. Circuit Breaker
        Self::check_price_deviation(&env, &token_address, price, config.max_price_deviation_bps);

        // 4. Escrow
        if side == Symbol::new(&env, "buy") {
            let total_cost = amount * price;
            let base_token = TokenClient::new(&env, &config.base_currency);
            base_token.transfer(&maker, &env.current_contract_address(), &total_cost);
        } else if side == Symbol::new(&env, "sell") {
            let rwa_token = RWATokenClient::new(&env, &token_address);
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
        env.storage().temporary().set(&DataKey::Order(count), &order);

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
            .unwrap_or_else(|| { panic_with_error!(&env, MarketError::OrderNotFound); });

        if order.expires_at <= env.ledger().timestamp() {
            if order.is_active {
                env.events().publish(
                    (Symbol::new(&env, "order_expired"), order.token_address.clone()),
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

        // Settlement
        let settlement_value = fill_amount * order.price;
        let fee = (settlement_value * config.fee_rate_bps as i128) / 10000;
        let net_value = settlement_value - fee;

        let base_token = TokenClient::new(&env, &config.base_currency);
        let rwa_token = RWATokenClient::new(&env, &order.token_address);

        if order.side == Symbol::new(&env, "buy") {
            // Maker was buyer (escrowed payment), Taker is seller
            // Transfer RWA from Taker to Maker
            rwa_token.transfer(&taker, &order.maker, &fill_amount);
            // Transfer payment from Escrow to Taker
            base_token.transfer(&env.current_contract_address(), &taker, &net_value);
        } else {
            // Maker was seller (escrowed tokens), Taker is buyer
            // Transfer RWA from Escrow to Taker
            rwa_token.transfer(&env.current_contract_address(), &taker, &fill_amount);
            // Transfer payment from Taker to Maker
            base_token.transfer(&taker, &order.maker, &net_value);
        }

        // Transfer fee
        if fee > 0 {
            if order.side == Symbol::new(&env, "buy") {
                base_token.transfer(&env.current_contract_address(), &config.fee_recipient, &fee);
            } else {
                base_token.transfer(&taker, &config.fee_recipient, &fee);
            }
        }

        // Update Order
        order.filled_amount += fill_amount;
        if order.filled_amount == order.amount {
            order.is_active = false;
        }
        env.storage().temporary().set(&DataKey::Order(order_id), &order);

        // Issue #141: a fully-filled order leaves the book.
        if !order.is_active {
            Self::book_remove(&env, &order);
        }

        // Record Trade & Update VWAP/TWAP
        Self::handle_trade_data(&env, order.token_address.clone(), fill_amount, order.price);

        env.events().publish(
            (Symbol::new(&env, "trade"), order.token_address),
            (order_id, taker, fill_amount, order.price, env.ledger().timestamp()),
        );
    }

    pub fn cancel_order(env: Env, maker: Address, order_id: u64) {
        maker.require_auth();

        Self::check_version(&env);

        let mut order: Order = env
            .storage()
            .temporary()
            .get(&DataKey::Order(order_id))
            .unwrap_or_else(|| { panic_with_error!(&env, MarketError::OrderNotFound); });

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
        env.storage().temporary().set(&DataKey::Order(order_id), &order);

        // Issue #141: drop the cancelled order from the book.
        Self::book_remove(&env, &order);

        env.events().publish(
            (Symbol::new(&env, "order_cancelled"), order.token_address),
            (order_id, maker, order.side, order.filled_amount, env.ledger().timestamp()),
        );
    }

    pub fn prune_expired_orders(env: Env) {
        Self::auto_prune_expired_orders(&env);
    }

    fn auto_prune_expired_orders(env: &Env) {
        let count: u64 = env.storage().instance().get(&DataKey::OrderCount).unwrap_or(0);
        if count == 0 {
            return;
        }
        let config: MarketConfig = match env.storage().instance().get(&DataKey::Config) {
            Some(c) => c,
            None => return,
        };
        let now = env.ledger().timestamp();
        for id in 1..=count {
            if let Some(mut order) = env.storage().temporary().get::<DataKey, Order>(&DataKey::Order(id)) {
                if order.is_active && order.expires_at <= now {
                    let remaining = order.amount - order.filled_amount;
                    if remaining > 0 {
                        if order.side == Symbol::new(env, "buy") {
                            let refund = remaining * order.price;
                            let base_token = TokenClient::new(env, &config.base_currency);
                            base_token.transfer(&env.current_contract_address(), &order.maker, &refund);
                        } else if order.side == Symbol::new(env, "sell") {
                            let rwa_token = RWATokenClient::new(env, &order.token_address);
                            rwa_token.transfer(&env.current_contract_address(), &order.maker, &remaining);
                        }
                    }
                    order.is_active = false;
                    env.storage().temporary().set(&DataKey::Order(id), &order);
                    // Issue #141: pruned orders leave the book.
                    Self::book_remove(env, &order);
                    env.events().publish(
                        (Symbol::new(env, "order_expired"), order.token_address.clone()),
                        (id, order.maker.clone(), now),
                    );
                }
            }
        }
    }

    // Helper: VWAP & TWAP Recording
    fn handle_trade_data(env: &Env, token: Address, amount: i128, price: i128) {
        let mut trade_count: u64 = env.storage().instance().get(&DataKey::TradeCount).unwrap_or(0);
        trade_count += 1;
        env.storage().instance().set(&DataKey::TradeCount, &trade_count);

        let now = env.ledger().timestamp();

        // 1. Update VWAP (Volume Weighted Average Price)
        // Store as (total_volume, total_value)
        let vwap_key = DataKey::VWAP(token.clone());
        let (mut total_vol, mut total_val): (i128, i128) = env
            .storage()
            .persistent()
            .get(&vwap_key)
            .unwrap_or((0, 0));
        
        total_vol += amount;
        total_val += amount * price;
        env.storage().persistent().set(&vwap_key, &(total_vol, total_val));

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

        env.storage().persistent().set(&twap_key, &(cum_price_time, price, now));
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
        env.storage().persistent().get(&key).unwrap_or(Vec::new(env))
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
            let move_right = if is_bid { order.price < p } else { order.price > p };
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
        let order: Order = env
            .storage()
            .temporary()
            .get(&DataKey::Order(id))?;
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
        let kyc = registry.get_kyc_status(user);
        if !kyc.is_verified {
            panic_with_error!(env, MarketError::ComplianceFailed);
        }
    }

    fn check_price_deviation(env: &Env, token: &Address, price: i128, max_deviation_bps: i64) {
        let vwap = env.storage().persistent().get(&DataKey::VWAP(token.clone())).unwrap_or(0);
        if vwap > 0 {
            let deviation = if price > vwap { price - vwap } else { vwap - price };
            let deviation_bps = (deviation * 10000) / vwap;
            if deviation_bps > max_deviation_bps as i128 {
                panic_with_error!(env, MarketError::CircuitBreakerTripped);
            }
        }
    }
}
