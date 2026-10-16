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
    // Issue #140: market orders.
    SlippageExceeded = 17,
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

        let settlement_value = fill_amount * order.price;
        let fee = (settlement_value * config.fee_rate_bps as i128) / 10000;
        let net_value = settlement_value - fee;

        let base_token = TokenClient::new(env, &config.base_currency);
        let rwa_token = RWATokenClient::new(env, &order.token_address);

        if order.side == Symbol::new(env, "buy") {
            // Maker was buyer (escrowed payment), taker is seller.
            rwa_token.transfer(taker, &order.maker, &fill_amount);
            base_token.transfer(&env.current_contract_address(), taker, &net_value);
        } else {
            // Maker was seller (escrowed tokens), taker is buyer.
            rwa_token.transfer(&env.current_contract_address(), taker, &fill_amount);
            base_token.transfer(taker, &order.maker, &net_value);
        }

        if fee > 0 {
            if order.side == Symbol::new(env, "buy") {
                base_token.transfer(&env.current_contract_address(), &config.fee_recipient, &fee);
            } else {
                base_token.transfer(taker, &config.fee_recipient, &fee);
            }
        }

        order.filled_amount += fill_amount;
        if order.filled_amount == order.amount {
            order.is_active = false;
        }
        env.storage().temporary().set(&DataKey::Order(order_id), order);

        Self::handle_trade_data(env, order.token_address.clone(), fill_amount, order.price);

        env.events().publish(
            (Symbol::new(env, "trade"), order.token_address.clone()),
            (order_id, taker.clone(), fill_amount, order.price, env.ledger().timestamp()),
        );
    }

    /// Best active resting order on one side of the book, ranked by price then
    /// arrival time (issue #140 scan-based helper).
    fn best_opposing_order(env: &Env, token_address: &Address, resting_is_bid: bool) -> Option<Order> {
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

        // The taker matches the opposite side of the book.
        let resting_is_bid = is_sell;

        env.events().publish(
            (Symbol::new(&env, "market_order_placed"), token_address.clone()),
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
        let best = match Self::best_opposing_order(&env, &token_address, resting_is_bid) {
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
            let mut order = match Self::best_opposing_order(&env, &token_address, resting_is_bid) {
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
                (Symbol::new(&env, "market_order_filled"), token_address.clone()),
                (order.id, fill, order.price, env.ledger().timestamp()),
            );

            remaining -= fill;
            filled_total += fill;
        }

        if filled_total == 0 {
            panic_with_error!(&env, MarketError::InsufficientLiquidity);
        }

        filled_total
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
