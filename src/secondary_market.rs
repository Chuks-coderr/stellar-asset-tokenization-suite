use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, token::TokenClient, Address, Env, Map,
    Symbol, Vec, log,
};

use crate::rwa_token::RWATokenClient;
use crate::dividend_distributor::DividendDistributorClient;
use crate::compliance_registry::ComplianceRegistryClient;

const STORAGE_VERSION: u32 = 1;

/// Issue #142: maximum live conditional orders per user and token.
const MAX_CONDITIONAL_ORDERS_PER_USER: u32 = 50;

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
    // Issue #142: conditional orders.
    ConditionalOrderLimitReached = 18,
    ConditionalOrderNotFound = 19,
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

        // Record Trade & Update VWAP/TWAP
        Self::handle_trade_data(&env, order.token_address.clone(), fill_amount, order.price);

        // Issue #142: evaluate conditional orders after every trade execution.
        Self::evaluate_conditional_orders(env.clone(), order.token_address.clone());

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

        // Issue #142: remember the last traded price for trigger evaluation.
        env.storage()
            .persistent()
            .set(&DataKey::LastPrice(token.clone()), &price);

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
            let mut cond: ConditionalOrder = match env
                .storage()
                .instance()
                .get(&DataKey::ConditionalOrder(id))
            {
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
