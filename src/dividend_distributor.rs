use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, token::TokenClient,
    Address, Env, Map, Symbol, Vec,
};

use crate::rwa_token::RWATokenClient;
use crate::secondary_market::SecondaryMarketClient;

const STORAGE_VERSION: u32 = 1;

/// Default minimum claim window (seconds) enforced on new distributions.
/// Guarantees holders at least a 1-hour window to claim (issue #132).
/// Configurable per-deployment via `DividendConfig.min_claim_window`.
pub const DEFAULT_MIN_CLAIM_WINDOW: u64 = 3600;

/// Maximum allowed fee rate in basis points (1000 bps = 10%).
/// Enforced by `update_config` (issue #136).
pub const MAX_FEE_RATE: i64 = 1000;

/// Proposal key used by the governance committee to authorise lifting an
/// emergency freeze (`freeze_all_distributions`). See issue #139.
pub const UNFREEZE_PROPOSAL_KEY: &str = "unfreeze_distributions";

// ── Issue #127: Boost tier lock duration constants ───────────────────────────
pub const BOOST_TIER_30D: u64 = 30 * 24 * 3600;  // 1.1x => 1_100_000
pub const BOOST_TIER_90D: u64 = 90 * 24 * 3600;  // 1.5x => 1_500_000
pub const BOOST_TIER_180D: u64 = 180 * 24 * 3600; // 2.0x => 2_000_000
pub const BOOST_TIER_365D: u64 = 365 * 24 * 3600; // 3.0x => 3_000_000

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum DividendError {
    Unauthorized = 1,
    InsufficientFunds = 2,
    InvalidAmount = 3,
    DistributionNotFound = 4,
    AlreadyClaimed = 5,
    UnsupportedCurrency = 6,
    DistributionNotActive = 7,
    AlreadyInitialized = 8,
    LengthMismatch = 9,
    NotInitialized = 10,
    ConfigNotFound = 11,
    AutoDistributionDisabled = 12,
    YieldCadenceNotReached = 13,
    ZeroTotalSupply = 14,
    NoTokensToClaim = 15,
    NoDividendAvailable = 16,
    StorageOutdated = 17,
    DistributorNotInitialized = 18,
    AlreadyAtLatestVersion = 19,
    InvalidParameters = 20,
    NoAccrualConfigured = 21,
    ClaimDeadlineTooSoon = 22,
    // Issue #139: emergency freeze errors.
    DistributionsFrozen = 23,
    InvalidGovernanceProposal = 24,
    // Issue #125: dividend reinvestment errors.
    MarketNotConfigured = 25,
    ReinvestmentFailed = 26,
}

/// Issue #126: DividendDistribution now carries `start_time` and `end_time`
/// for time-weighted pro-rata calculations.
#[contracttype]
#[derive(Clone, Debug)]
pub struct DividendDistribution {
    pub distribution_id: u64,
    pub token_address: Address,
    pub currency: Symbol,
    pub total_amount: i128,
    pub per_token_amount: i128,
    pub total_supply: i128,
    pub claim_deadline: u64,
    pub created_at: u64,
    pub is_active: bool,
    pub metadata: Map<Symbol, Symbol>,
    /// Issue #126: accrual period start (= created_at).
    pub start_time: u64,
    /// Issue #126: accrual period end (= claim_deadline).
    pub end_time: u64,
}

#[contracttype]
#[derive(Clone)]
pub struct ClaimKey {
    pub distribution_id: u64,
    pub claimer: Address,
}

#[contracttype]
#[derive(Clone)]
pub struct ClaimInfo {
    pub distribution_id: u64,
    pub claimer: Address,
    pub amount_claimed: i128,
    pub claimed_at: u64,
    pub currency: Symbol,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaimState {
    Unclaimed,
    InProgress,
    Claimed,
}

#[contracttype]
#[derive(Clone)]
pub struct AccrualRate {
    pub token_address: Address,
    pub currency: Symbol,
    pub amount_per_second: i128,
    pub last_accrual_update: u64,
    pub total_accrued: i128,
}

#[contracttype]
#[derive(Clone)]
pub struct AccrualClaimKey {
    pub token_address: Address,
    pub claimer: Address,
}

/// Issue #131: cron-like recurring distribution schedule.
///
/// `minute`, `hour`, `day_of_month`, `month`, and `day_of_week` use `-1` as a
/// wildcard meaning "every". A schedule is due when all fields match the
/// current ledger timestamp.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecurringSchedule {
    pub minute: i64,
    pub hour: i64,
    pub day_of_month: i64,
    pub month: i64,
    pub day_of_week: i64,
    /// Last timestamp this schedule fired (0 means never).
    pub last_fired_at: u64,
    /// Interval in seconds between allowed firings (min spacing guard).
    pub min_interval_seconds: u64,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct DistributionHistoryPage {
    pub distributions: Vec<DividendDistribution>,
    pub total: u64,
    pub offset: u64,
    pub limit: u64,
    pub has_more: bool,
}

#[contracttype]
#[derive(Clone)]
pub struct DividendConfig {
    pub supported_currencies: Vec<Symbol>,
    pub auto_distribute: bool,
    pub min_distribution_amount: i128,
    pub max_distribution_frequency: u64,
    pub fee_rate: i64,
    pub fee_recipient: Address,
    /// Minimum seconds between distribution creation and its claim deadline.
    /// `create_distribution` rejects deadlines sooner than
    /// `now + min_claim_window` with `ClaimDeadlineTooSoon` (issue #132).
    /// Set to 0 to disable the window check.
    pub min_claim_window: u64,
}

/// Issue #127: Per-token boost configuration for yield farming multipliers.
#[contracttype]
#[derive(Clone)]
pub struct BoostConfig {
    /// Base multiplier expressed as fixed-point 1_000_000 = 1×.
    pub base_rate: i128,
    /// Additional multiplier per lock tier (informational; tiers are
    /// hard-coded as constants above).
    pub lock_boost_multiplier: i128,
    /// Maximum allowed multiplier, e.g. 3_000_000 = 3×.
    pub max_boost: i128,
}

#[contract]
pub struct DividendDistributor;

#[contractimpl]
impl DividendDistributor {
    pub fn initialize(env: Env, auth: Address, admin: Address, supported_currencies: Vec<Symbol>) {
        auth.require_auth();
        if env
            .storage()
            .instance()
            .has(&Symbol::new(&env, "initialized"))
        {
            panic_with_error!(&env, DividendError::AlreadyInitialized);
        }

        let config = DividendConfig {
            supported_currencies: supported_currencies.clone(),
            auto_distribute: false,
            min_distribution_amount: 1000,
            max_distribution_frequency: 86400,
            fee_rate: 50,
            fee_recipient: admin.clone(),
            min_claim_window: DEFAULT_MIN_CLAIM_WINDOW,
        };

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "admin"), &admin);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "config"), &config);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "initialized"), &true);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "distribution_count"), &0u64);
        // Issue #130: store distributions as Map<u64, DividendDistribution>
        // for O(1) lookup instead of O(n) Vec scan.
        env.storage().instance().set(
            &Symbol::new(&env, "distributions"),
            &Map::<u64, DividendDistribution>::new(&env),
        );
        env.storage().instance().set(
            &Symbol::new(&env, "currency_tokens"),
            &Map::<Symbol, Address>::new(&env),
        );
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "last_auto_distribution_at"), &0u64);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "clm_tot"), &Map::<u64, i128>::new(&env));
        env.storage().instance().set(
            &Symbol::new(&env, "accruals"),
            &Map::<Address, AccrualRate>::new(&env),
        );
        env.storage().instance().set(
            &Symbol::new(&env, "registered_tokens"),
            &Vec::<Address>::new(&env),
        );
        // Issue #126: holder snapshot map (Address -> timestamp).
        env.storage().instance().set(
            &Symbol::new(&env, "holder_snap"),
            &Map::<Address, u64>::new(&env),
        );
        // Issue #127: per-token boost configs (Address -> BoostConfig).
        env.storage().instance().set(
            &Symbol::new(&env, "boost_configs"),
            &Map::<Address, BoostConfig>::new(&env),
        );
        // Issue #127: per-holder registered lock durations (Address -> u64 seconds).
        env.storage().instance().set(
            &Symbol::new(&env, "lock_durations"),
            &Map::<Address, u64>::new(&env),
        );
        // Issue #139: no emergency freeze is active at deployment time.
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "distributions_frozen"), &false);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "version"), &STORAGE_VERSION);
        // Issue #134: claim state machine storage.
        env.storage()
            .instance()
            .set(
                &Symbol::new(&env, "claim_states"),
                &Map::<ClaimKey, ClaimState>::new(&env),
            );
        // Issue #131: recurring schedule storage.
        env.storage()
            .instance()
            .set(
                &Symbol::new(&env, "recurring_schedules"),
                &Map::<u64, RecurringSchedule>::new(&env),
            );
    }

    fn read_version(env: &Env) -> u32 {
        env.storage()
            .instance()
            .get(&Symbol::new(env, "version"))
            .unwrap_or(0)
    }

    fn check_version(env: &Env) {
        if Self::read_version(env) < STORAGE_VERSION {
            panic_with_error!(env, DividendError::StorageOutdated);
        }
    }

    pub fn migrate(env: Env, auth: Address) {
        crate::shared_admin::require_admin(&env, &auth);

        let ver = Self::read_version(&env);
        if ver >= STORAGE_VERSION {
            panic_with_error!(&env, DividendError::AlreadyAtLatestVersion);
        }

        let mut current = ver;
        while current < STORAGE_VERSION {
            current += 1;
        }

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "version"), &STORAGE_VERSION);
    }

    pub fn register_currency_token(
        env: Env,
        auth: Address,
        currency: Symbol,
        token_address: Address,
    ) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let mut map: Map<Symbol, Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "currency_tokens"))
            .unwrap_or(Map::new(&env));

        map.set(currency, token_address);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "currency_tokens"), &map);
    }

    pub fn register_token(env: Env, auth: Address, token_address: Address) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        Self::register_token_internal(env.clone(), token_address.clone());

        env.events().publish(
            (Symbol::new(&env, "token_registered"), token_address),
            (),
        );
    }

    pub fn multi_ccy_distributions(
        env: Env,
        auth: Address,
        token_address: Address,
        currencies: Vec<Symbol>,
        amounts: Vec<i128>,
        claim_deadline: u64,
        metadata: Map<Symbol, Symbol>,
    ) -> Vec<u64> {
        if currencies.len() != amounts.len() {
            panic_with_error!(&env, DividendError::LengthMismatch);
        }

        let mut ids = Vec::<u64>::new(&env);
        let mut i: u32 = 0;
        while i < currencies.len() {
            let currency = currencies.get(i).unwrap();
            let amount = amounts.get(i).unwrap();
            let distribution_id = Self::create_distribution(
                env.clone(),
                auth.clone(),
                token_address.clone(),
                currency,
                amount,
                claim_deadline,
                metadata.clone(),
                DistributionCategory::Regular,
            );
            ids.push_back(distribution_id);
            i += 1;
        }
        ids
    }

    pub fn auto_yield_distribute(
        env: Env,
        auth: Address,
        token_address: Address,
        currencies: Vec<Symbol>,
        amounts: Vec<i128>,
        claim_deadline: u64,
        metadata: Map<Symbol, Symbol>,
    ) -> Vec<u64> {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let config: DividendConfig = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "config"))
            .unwrap_or_else(|| {
                panic_with_error!(&env, DividendError::ConfigNotFound);
            });

        if !config.auto_distribute {
            panic_with_error!(&env, DividendError::AutoDistributionDisabled);
        }

        let now = env.ledger().timestamp();
        let last: u64 = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "last_auto_distribution_at"))
            .unwrap_or(0u64);

        if last > 0 && now.saturating_sub(last) < config.max_distribution_frequency {
            panic_with_error!(&env, DividendError::YieldCadenceNotReached);
        }

        let ids = Self::multi_ccy_distributions(
            env.clone(),
            auth.clone(),
            token_address,
            currencies,
            amounts,
            claim_deadline,
            metadata,
        );

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "last_auto_distribution_at"), &now);

        ids
    }

    pub fn create_distribution(
        env: Env,
        auth: Address,
        token_address: Address,
        currency: Symbol,
        amount: i128,
        claim_deadline: u64,
        metadata: Map<Symbol, Symbol>,
        category: DistributionCategory,
    ) -> u64 {
        if amount <= 0 {
            panic_with_error!(&env, DividendError::InvalidAmount);
        }

        let now = env.ledger().timestamp();
        if claim_deadline <= now {
            panic_with_error!(&env, DividendError::InvalidParameters);
        }

        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        // Issue #139: an active emergency freeze blocks new distributions.
        if Self::is_frozen(&env) {
            panic_with_error!(&env, DividendError::DistributionsFrozen);
        }

        let config: DividendConfig = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "config"))
            .unwrap_or_else(|| {
                panic_with_error!(&env, DividendError::ConfigNotFound);
            });

        // Issue #132 — require a minimum claim window so distributions cannot
        // be created with a deadline holders cannot realistically meet.
        if claim_deadline.saturating_sub(now) < config.min_claim_window {
            panic_with_error!(&env, DividendError::ClaimDeadlineTooSoon);
        }

        if !config
            .supported_currencies
            .iter()
            .any(|c| c.clone() == currency)
        {
            panic_with_error!(&env, DividendError::UnsupportedCurrency);
        }

        if amount < config.min_distribution_amount {
            panic_with_error!(&env, DividendError::InvalidAmount);
        }

        let rwa_client = RWATokenClient::new(&env, &token_address);
        let total_supply = rwa_client.get_token_info().total_supply;

        // Issue #128: reject distributions when the token has no supply.
        if total_supply == 0 {
            panic_with_error!(&env, DividendError::ZeroTotalSupply);
        }

        let per_token_amount = (amount * 1_000_000_000_000_000_000i128) / total_supply;

        let distribution_count: u64 = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distribution_count"))
            .unwrap_or(0u64);

        let distribution_id = distribution_count + 1;

        // Issue #126: record start/end times for time-weighted calculations.
        let start_time = now;
        let end_time = claim_deadline;

        let distribution = DividendDistribution {
            distribution_id,
            token_address: token_address.clone(),
            currency: currency.clone(),
            total_amount: amount,
            per_token_amount,
            total_supply,
            claim_deadline,
            created_at: now,
            is_active: true,
            metadata,
            start_time,
            end_time,
        };

        // Issue #130: use Map<u64, DividendDistribution> for O(1) access.
        let mut distributions: Map<u64, DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Map::new(&env));
        distributions.set(distribution_id, distribution);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "distributions"), &distributions);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "distribution_count"), &distribution_id);

        let currency_token_address =
            Self::get_currency_token_address(env.clone(), currency.clone());
        let currency_client = TokenClient::new(&env, &currency_token_address);

        currency_client.transfer(&auth, &env.current_contract_address(), &amount);

        // Track the token so portfolio-wide claims can iterate all held assets.
        Self::register_token_internal(env.clone(), token_address.clone());

        env.events().publish(
            (Symbol::new(&env, "distribution_created"), token_address),
            (distribution_id, amount, currency, now),
        );

        distribution_id
    }

    // ── Issue #126: Holder snapshot recording ────────────────────────────────

    /// Record the current ledger timestamp as the moment `holder` entered the
    /// token position.  Call this once when a holder first acquires tokens so
    /// that `claim_dividend` can apply the correct time-weight.
    ///
    /// If not called, `claim_dividend` treats the holder as a full-period
    /// holder (snapshot_time = start_time ⇒ 100% weight).
    pub fn record_holding_snapshot(env: Env, holder: Address, token_address: Address) {
        // The holder (or an admin) must authorise this call to prevent griefing.
        holder.require_auth();

        // Suppress unused-variable warning; token_address is part of the
        // public API so that callers can be specific about which token.
        let _ = token_address;

        let mut snap_map: Map<Address, u64> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "holder_snap"))
            .unwrap_or(Map::new(&env));

        // Only record the *first* snapshot; subsequent calls are no-ops.
        if snap_map.get(holder.clone()).is_none() {
            snap_map.set(holder.clone(), env.ledger().timestamp());
            env.storage()
                .instance()
                .set(&Symbol::new(&env, "holder_snap"), &snap_map);
        }
    }

    /// Compute the time-weight (0..=1_000_000) for `holder` relative to the
    /// given distribution period.
    ///
    /// Full-period holders (snapshot ≤ start_time) receive 1_000_000 (100%).
    /// Holders with no snapshot are treated as full-period holders.
    fn compute_time_weight(
        env: &Env,
        holder: &Address,
        start_time: u64,
        end_time: u64,
        current_time: u64,
    ) -> i128 {
        let period_duration = end_time.saturating_sub(start_time);
        if period_duration == 0 {
            return 0;
        }

        let snap_map: Map<Address, u64> = env
            .storage()
            .instance()
            .get(&Symbol::new(env, "holder_snap"))
            .unwrap_or(Map::new(env));

        // Default snapshot = start_time so holders without a snapshot receive
        // full weight (backwards-compatible behaviour).
        let snapshot_time = snap_map
            .get(holder.clone())
            .unwrap_or(start_time);

        // If the snapshot was recorded before the period started, treat as
        // full-period holder.
        let effective_snapshot = if snapshot_time <= start_time {
            start_time
        } else {
            snapshot_time
        };

        let effective_end = if current_time < end_time {
            current_time
        } else {
            end_time
        };

        if effective_end <= effective_snapshot {
            return 0;
        }

        let holding_duration = effective_end - effective_snapshot;
        // Fixed-point: max 1_000_000
        let weight = (holding_duration as i128) * 1_000_000i128 / (period_duration as i128);
        if weight > 1_000_000 {
            1_000_000
        } else {
            weight
        }
    }

    // ── Issue #127: Boost multiplier helpers ──────────────────────────────────

    /// Store per-token boost configuration.  Admin-only.
    pub fn set_boost_config(env: Env, auth: Address, token_address: Address, config: BoostConfig) {
        crate::shared_admin::require_admin(&env, &auth);
        Self::check_version(&env);

        let mut boost_configs: Map<Address, BoostConfig> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "boost_configs"))
            .unwrap_or(Map::new(&env));

        boost_configs.set(token_address, config);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "boost_configs"), &boost_configs);
    }

    /// Read per-token boost configuration.
    pub fn get_boost_config(env: Env, token_address: Address) -> Option<BoostConfig> {
        let boost_configs: Map<Address, BoostConfig> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "boost_configs"))
            .unwrap_or(Map::new(&env));
        boost_configs.get(token_address)
    }

    /// Register the lock duration (in seconds) for a holder so the distributor
    /// can compute the correct boost tier without needing a cross-contract call
    /// into RWAToken's internal lock storage.
    ///
    /// The holder or admin should call this immediately after locking tokens.
    pub fn register_lock_duration(
        env: Env,
        holder: Address,
        token_address: Address,
        lock_seconds: u64,
    ) {
        holder.require_auth();

        // token_address is part of the API; suppress unused-variable warning.
        let _ = token_address;

        let mut lock_durations: Map<Address, u64> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "lock_durations"))
            .unwrap_or(Map::new(&env));

        lock_durations.set(holder, lock_seconds);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "lock_durations"), &lock_durations);
    }

    /// Compute the boost multiplier (fixed-point, 1_000_000 = 1×) for `holder`
    /// on `token_address`.  Returns 1_000_000 when no lock or no boost config.
    fn compute_boost_multiplier(env: &Env, holder: Address, token_address: Address) -> i128 {
        // No boost config registered for this token → no boost.
        let boost_configs: Map<Address, BoostConfig> = env
            .storage()
            .instance()
            .get(&Symbol::new(env, "boost_configs"))
            .unwrap_or(Map::new(env));

        let config = match boost_configs.get(token_address) {
            Some(c) => c,
            None => return 1_000_000,
        };

        // Look up the holder's registered lock duration.
        let lock_durations: Map<Address, u64> = env
            .storage()
            .instance()
            .get(&Symbol::new(env, "lock_durations"))
            .unwrap_or(Map::new(env));

        let lock_seconds = lock_durations.get(holder).unwrap_or(0u64);

        let raw_multiplier: i128 = if lock_seconds >= BOOST_TIER_365D {
            3_000_000
        } else if lock_seconds >= BOOST_TIER_180D {
            2_000_000
        } else if lock_seconds >= BOOST_TIER_90D {
            1_500_000
        } else if lock_seconds >= BOOST_TIER_30D {
            1_100_000
        } else {
            1_000_000
        };

        // Cap at max_boost.
        if raw_multiplier > config.max_boost {
            config.max_boost
        } else {
            raw_multiplier
        }
    }

    /// Claim dividend with yield-farming boost applied.
    ///
    /// Identical to `claim_dividend` but multiplies `claimable_amount` by the
    /// holder's boost multiplier before deducting fees.
    pub fn claim_boosted_dividend(env: Env, distribution_id: u64, claimer: Address) -> i128 {
        Self::check_version(&env);

        // Issue #139: claims are disabled while an emergency freeze is active.
        if Self::is_frozen(&env) {
            panic_with_error!(&env, DividendError::DistributionNotActive);
        }

        // Issue #130: O(1) map lookup.
        let distributions: Map<u64, DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Map::new(&env));

        let distribution = distributions
            .get(distribution_id)
            .unwrap_or_else(|| panic_with_error!(&env, DividendError::DistributionNotFound));

        if !distribution.is_active {
            panic_with_error!(&env, DividendError::DistributionNotActive);
        }

        let current_time = env.ledger().timestamp();
        if current_time > distribution.claim_deadline {
            panic_with_error!(&env, DividendError::DistributionNotActive);
        }

        let ck = ClaimKey {
            distribution_id,
            claimer: claimer.clone(),
        };
        if env.storage().instance().has(&ck) {
            panic_with_error!(&env, DividendError::AlreadyClaimed);
        }

        let rwa_client = RWATokenClient::new(&env, &distribution.token_address);
        let balance = rwa_client.get_balance(&claimer).amount;

        if balance == 0 {
            panic_with_error!(&env, DividendError::NoTokensToClaim);
        }

        let base_claimable =
            (balance * distribution.per_token_amount) / 1_000_000_000_000_000_000i128;

        if base_claimable <= 0 {
            panic_with_error!(&env, DividendError::NoDividendAvailable);
        }

        // Issue #127: apply boost multiplier.
        let multiplier =
            Self::compute_boost_multiplier(&env, claimer.clone(), distribution.token_address.clone());
        let boosted_amount = (base_claimable * multiplier) / 1_000_000i128;

        let config: DividendConfig = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "config"))
            .unwrap_or_else(|| panic_with_error!(&env, DividendError::ConfigNotFound));

        let fee_amount = (boosted_amount * config.fee_rate as i128) / 10_000i128;
        let net_amount = boosted_amount - fee_amount;

        let dist_currency = distribution.currency.clone();
        let currency_token_address =
            Self::get_currency_token_address(env.clone(), dist_currency.clone());
        let currency_client = TokenClient::new(&env, &currency_token_address);

        currency_client.transfer(&env.current_contract_address(), &claimer, &net_amount);

        if fee_amount > 0 {
            currency_client.transfer(
                &env.current_contract_address(),
                &config.fee_recipient,
                &fee_amount,
            );
        }

        let claim_info = ClaimInfo {
            distribution_id,
            claimer: claimer.clone(),
            amount_claimed: net_amount,
            claimed_at: current_time,
            currency: dist_currency.clone(),
        };

        env.storage().instance().set(&ck, &claim_info);

        let tot_key = Symbol::new(&env, "clm_tot");
        let mut totals: Map<u64, i128> = env
            .storage()
            .instance()
            .get(&tot_key)
            .unwrap_or(Map::new(&env));
        let prev = totals.get(distribution_id).unwrap_or(0i128);
        totals.set(distribution_id, prev + net_amount);
        env.storage().instance().set(&tot_key, &totals);

        env.events().publish(
            (Symbol::new(&env, "boosted_dividend_claimed"), claimer),
            (distribution_id, net_amount, multiplier, dist_currency, current_time),
        );

        net_amount
    }

    pub fn claim_dividend(env: Env, distribution_id: u64, claimer: Address) -> i128 {
        Self::check_version(&env);

        // Issue #139: claims are disabled while an emergency freeze is active.
        if Self::is_frozen(&env) {
            panic_with_error!(&env, DividendError::DistributionNotActive);
        }

        // Issue #130: O(1) map lookup instead of O(n) Vec scan.
        let distributions: Map<u64, DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Map::new(&env));

        let distribution = distributions
            .get(distribution_id)
            .unwrap_or_else(|| panic_with_error!(&env, DividendError::DistributionNotFound));

        if !distribution.is_active {
            panic_with_error!(&env, DividendError::DistributionNotActive);
        }

        let current_time = env.ledger().timestamp();
        if current_time > distribution.claim_deadline {
            panic_with_error!(&env, DividendError::DistributionNotActive);
        }

        let ck = ClaimKey {
            distribution_id,
            claimer: claimer.clone(),
        };
        // Issue #134: claim state machine — reject already-claimed or in-progress claims.
        let claim_state: ClaimState = env
            .storage()
            .instance()
            .get(&ck)
            .unwrap_or(ClaimState::Unclaimed);
        match claim_state {
            ClaimState::Claimed => panic_with_error!(&env, DividendError::AlreadyClaimed),
            ClaimState::InProgress => panic_with_error!(&env, DividendError::InvalidParameters),
            ClaimState::Unclaimed => {}
        }

        let rwa_client = RWATokenClient::new(&env, &distribution.token_address);
        let balance = rwa_client.get_balance(&claimer).amount;

        if balance == 0 {
            panic_with_error!(&env, DividendError::NoTokensToClaim);
        }

        let claimable_amount =
            (balance * distribution.per_token_amount) / 1_000_000_000_000_000_000i128;

        if claimable_amount <= 0 {
            panic_with_error!(&env, DividendError::NoDividendAvailable);
        }

        // Issue #126: apply time-weighting for partial-period holders.
        let time_weight = Self::compute_time_weight(
            &env,
            &claimer,
            distribution.start_time,
            distribution.end_time,
            current_time,
        );
        let time_weighted_amount = (claimable_amount * time_weight) / 1_000_000i128;

        if time_weighted_amount <= 0 {
            panic_with_error!(&env, DividendError::NoDividendAvailable);
        }

        let config: DividendConfig = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "config"))
            .unwrap_or_else(|| panic_with_error!(&env, DividendError::ConfigNotFound));

        let fee_amount = (time_weighted_amount * config.fee_rate as i128) / 10_000i128;
        let net_amount = time_weighted_amount - fee_amount;

        let dist_currency = distribution.currency.clone();
        let currency_token_address =
            Self::get_currency_token_address(env.clone(), dist_currency.clone());
        let currency_client = TokenClient::new(&env, &currency_token_address);

        // Issue #125: If the claimer has opted in to dividend reinvestment,
        // attempt to auto-purchase RWA tokens at the current VWAP instead of
        // paying out in currency. Falls back to a normal currency payout when
        // no market is configured, VWAP is zero (no liquidity), or the
        // computed token amount is zero.
        let token_address = distribution.token_address.clone();
        let reinvesting = Self::is_reinvesting(env.clone(), claimer.clone(), token_address.clone());
        let mut reinvested = false;

        if reinvesting {
            let market_addr: Option<Address> = env
                .storage()
                .instance()
                .get(&Symbol::new(&env, "market_addr"));

            if let Some(market_addr) = market_addr {
                let market_client = SecondaryMarketClient::new(&env, &market_addr);
                let vwap = market_client.get_vwap(&token_address);

                if vwap > 0 {
                    let tokens_bought = net_amount / vwap;
                    if tokens_bought > 0 {
                        // Use the dividend currency to pay for the token
                        // purchase (currency stays with the contract) and mint
                        // new RWA tokens to the claimer.
                        let rwa_client = RWATokenClient::new(&env, &token_address);
                        rwa_client.mint(&env.current_contract_address(), &claimer, &tokens_bought);

                        // Pay the fee in currency as usual.
                        if fee_amount > 0 {
                            currency_client.transfer(
                                &env.current_contract_address(),
                                &config.fee_recipient,
                                &fee_amount,
                            );
                        }

                        env.events().publish(
                            (
                                Symbol::new(&env, "reinvestment_executed"),
                                claimer.clone(),
                                token_address.clone(),
                            ),
                            (distribution_id, tokens_bought, net_amount, vwap, current_time),
                        );

                        reinvested = true;
                    }
                }
            }
        }

        if !reinvested {
            // Normal currency payout (also the fallback when reinvestment is
            // not possible — no market, no liquidity, or zero tokens).
            currency_client.transfer(&env.current_contract_address(), &claimer, &net_amount);

            if fee_amount > 0 {
                currency_client.transfer(
                    &env.current_contract_address(),
                    &config.fee_recipient,
                    &fee_amount,
                );
            }
        }

        let claim_info = ClaimInfo {
            distribution_id,
            claimer: claimer.clone(),
            amount_claimed: net_amount,
            claimed_at: current_time,
            currency: dist_currency.clone(),
        };

        env.storage().instance().set(&ck, &claim_info);

        // Issue #134: mark claim as Claimed after successful transfer.
        env.storage().instance().set(&ck, &ClaimState::Claimed);

        let tot_key = Symbol::new(&env, "clm_tot");
        let mut totals: Map<u64, i128> = env
            .storage()
            .instance()
            .get(&tot_key)
            .unwrap_or(Map::new(&env));
        let prev = totals.get(distribution_id).unwrap_or(0i128);
        totals.set(distribution_id, prev + net_amount);
        env.storage().instance().set(&tot_key, &totals);

        env.events().publish(
            (Symbol::new(&env, "time_weighted_dividend_claimed"), claimer.clone()),
            (distribution_id, net_amount, dist_currency.clone(), current_time),
        );
        env.events().publish(
            (Symbol::new(&env, "dividend_claimed"), claimer),
            (distribution_id, net_amount, dist_currency, current_time),
        );

        net_amount
    }

    pub fn claim_all_dividends(env: Env, claimer: Address) -> Vec<i128> {
        // Issue #139: no claims while frozen.
        if Self::is_frozen(&env) {
            return Vec::new(&env);
        }

        // Issue #130: iterate map values.
        let distributions: Map<u64, DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Map::new(&env));

        let mut claimed_amounts = Vec::<i128>::new(&env);
        let current_time = env.ledger().timestamp();

        for (_id, distribution) in distributions.iter() {
            if distribution.is_active && current_time <= distribution.claim_deadline {
                let ck = ClaimKey {
                    distribution_id: distribution.distribution_id,
                    claimer: claimer.clone(),
                };
                if !env.storage().instance().has(&ck) {
                    let available = Self::calculate_available_dividend(
                        env.clone(),
                        distribution.distribution_id,
                        claimer.clone(),
                    );
                    if available > 0 {
                        let claimed = Self::claim_dividend(
                            env.clone(),
                            distribution.distribution_id,
                            claimer.clone(),
                        );
                        claimed_amounts.push_back(claimed);
                    }
                }
            }
        }

        claimed_amounts
    }

    pub fn claim_all_portfolio_dividends(env: Env, claimer: Address) -> Vec<(Address, i128)> {
        Self::check_version(&env);

        // Issue #139: no claims while frozen.
        if Self::is_frozen(&env) {
            return Vec::new(&env);
        }

        let tokens: Vec<Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "registered_tokens"))
            .unwrap_or(Vec::new(&env));

        // Bound portfolio iteration to a fixed maximum number of tokens.
        let max_tokens: u32 = 20;
        let mut results = Vec::<(Address, i128)>::new(&env);
        let mut processed: u32 = 0;

        for token in tokens.iter() {
            if processed >= max_tokens {
                break;
            }
            processed += 1;

            let rwa_client = RWATokenClient::new(&env, &token);
            let balance = rwa_client.get_balance(&claimer).amount;
            if balance == 0 {
                continue;
            }

            let total_claimed =
                Self::claim_all_for_token(env.clone(), claimer.clone(), token.clone());
            if total_claimed > 0 {
                results.push_back((token.clone(), total_claimed));
            }
        }

        results
    }

    fn claim_all_for_token(env: Env, claimer: Address, token_address: Address) -> i128 {
        // Issue #139: no claims while frozen.
        if Self::is_frozen(&env) {
            return 0;
        }

        // Issue #130: iterate map values filtered by token_address.
        let distributions: Map<u64, DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Map::new(&env));

        let mut total: i128 = 0;
        let current_time = env.ledger().timestamp();

        for (_id, distribution) in distributions.iter() {
            if distribution.token_address != token_address {
                continue;
            }
            if distribution.is_active && current_time <= distribution.claim_deadline {
                let ck = ClaimKey {
                    distribution_id: distribution.distribution_id,
                    claimer: claimer.clone(),
                };
                if !env.storage().instance().has(&ck) {
                    let available = Self::calculate_available_dividend(
                        env.clone(),
                        distribution.distribution_id,
                        claimer.clone(),
                    );
                    if available > 0 {
                        let claimed = Self::claim_dividend(
                            env.clone(),
                            distribution.distribution_id,
                            claimer.clone(),
                        );
                        total += claimed;
                    }
                }
            }
        }

        total
    }

    pub fn get_distribution(env: Env, distribution_id: u64) -> DividendDistribution {
        // Issue #130: O(1) map lookup.
        let distributions: Map<u64, DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Map::new(&env));

        distributions
            .get(distribution_id)
            .unwrap_or_else(|| panic_with_error!(&env, DividendError::DistributionNotFound))
    }

    pub fn get_active_distributions(env: Env, token_address: Address) -> Vec<DividendDistribution> {
        // Issue #130: iterate map values.
        let distributions: Map<u64, DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Map::new(&env));

        let mut active_distributions = Vec::<DividendDistribution>::new(&env);
        let current_time = env.ledger().timestamp();

        for (_id, distribution) in distributions.iter() {
            if distribution.token_address == token_address
                && distribution.is_active
                && current_time <= distribution.claim_deadline
            {
                active_distributions.push_back(distribution.clone());
            }
        }

        active_distributions
    }

    /// Returns a paginated page of all distributions (active and inactive),
    /// ordered by creation time descending (newest first). `offset` skips the
    /// first `offset` entries; `limit` caps the page size.
    pub fn get_distribution_history(
        env: Env,
        offset: u64,
        limit: u64,
    ) -> DistributionHistoryPage {
        let distributions: Vec<DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Vec::new(&env));

        let total = distributions.len() as u64;

        // Clamp offset to total.
        let start = if offset >= total { total } else { offset };
        // Cap limit to a sane maximum to prevent runaway reads.
        let max_limit: u64 = 100;
        let page_limit = if limit == 0 || limit > max_limit { max_limit } else { limit };

        let mut page = Vec::<DividendDistribution>::new(&env);
        let mut end = start.saturating_add(page_limit);
        if end > total {
            end = total;
        }

        // Iterate in reverse so newest distributions appear first.
        let mut i = end;
        while i > start {
            i -= 1;
            let dist = distributions.get(i as u32).unwrap();
            page.push_back(dist);
        }

        DistributionHistoryPage {
            distributions: page,
            total,
            offset: start,
            limit: page_limit,
            has_more: end < total,
        }
    }

    pub fn get_claim_info(env: Env, distribution_id: u64, claimer: Address) -> Option<ClaimInfo> {
        let ck = ClaimKey {
            distribution_id,
            claimer,
        };
        env.storage().instance().get(&ck)
    }

    pub fn calculate_available_dividend(env: Env, distribution_id: u64, claimer: Address) -> i128 {
        // Issue #139: nothing is claimable while frozen.
        if Self::is_frozen(&env) {
            return 0;
        }

        // Issue #130: O(1) map lookup.
        let distributions: Map<u64, DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Map::new(&env));

        let distribution = match distributions.get(distribution_id) {
            Some(d) => d,
            None => panic_with_error!(&env, DividendError::DistributionNotFound),
        };

        if !distribution.is_active {
            return 0;
        }

        let current_time = env.ledger().timestamp();
        if current_time > distribution.claim_deadline {
            return 0;
        }

        let ck = ClaimKey {
            distribution_id,
            claimer: claimer.clone(),
        };
        if env.storage().instance().has(&ck) {
            return 0;
        }

        let rwa_client = RWATokenClient::new(&env, &distribution.token_address);
        let balance = rwa_client.get_balance(&claimer).amount;

        if balance == 0 {
            return 0;
        }

        let claimable_amount =
            (balance * distribution.per_token_amount) / 1_000_000_000_000_000_000i128;

        // Issue #126: apply time-weighting.
        let time_weight = Self::compute_time_weight(
            &env,
            &claimer,
            distribution.start_time,
            distribution.end_time,
            current_time,
        );
        let time_weighted_amount = (claimable_amount * time_weight) / 1_000_000i128;

        if time_weighted_amount <= 0 {
            return 0;
        }

        let config: DividendConfig = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "config"))
            .unwrap_or_else(|| panic_with_error!(&env, DividendError::ConfigNotFound));

        let fee_amount = (time_weighted_amount * config.fee_rate as i128) / 10_000i128;
        time_weighted_amount - fee_amount
    }

    pub fn update_accrual(
        env: Env,
        auth: Address,
        token_address: Address,
        currency: Symbol,
        amount_per_second: i128,
    ) {
        crate::shared_admin::require_admin(&env, &auth);
        Self::check_version(&env);

        let config: DividendConfig = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "config"))
            .unwrap_or_else(|| panic_with_error!(&env, DividendError::ConfigNotFound));

        if !config
            .supported_currencies
            .iter()
            .any(|c| c.clone() == currency)
        {
            panic_with_error!(&env, DividendError::UnsupportedCurrency);
        }

        let mut accruals: Map<Address, AccrualRate> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "accruals"))
            .unwrap_or(Map::new(&env));

        let now = env.ledger().timestamp();
        let total_accrued = if let Some(existing) = accruals.get(token_address.clone()) {
            Self::settle_accrual(&env, &existing, now)
        } else {
            0
        };

        let new_rate = AccrualRate {
            token_address: token_address.clone(),
            currency,
            amount_per_second,
            last_accrual_update: now,
            total_accrued,
        };

        accruals.set(token_address, new_rate);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "accruals"), &accruals);
    }

    fn settle_accrual(env: &Env, rate: &AccrualRate, now: u64) -> i128 {
        if now <= rate.last_accrual_update {
            return rate.total_accrued;
        }
        let elapsed = (now - rate.last_accrual_update) as i128;
        rate.total_accrued + (rate.amount_per_second * elapsed)
    }

    pub fn calculate_accrued_yield(env: Env, claimer: Address, token_address: Address) -> i128 {
        let accruals: Map<Address, AccrualRate> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "accruals"))
            .unwrap_or(Map::new(&env));

        let rate = match accruals.get(token_address.clone()) {
            Some(r) => r,
            None => return 0,
        };

        let now = env.ledger().timestamp();
        let accrued_total = Self::settle_accrual(&env, &rate, now);

        let rwa_client = RWATokenClient::new(&env, &token_address);
        let balance = rwa_client.get_balance(&claimer).amount;
        if balance == 0 {
            return 0;
        }

        let ack = AccrualClaimKey {
            token_address,
            claimer,
        };
        let last_claimed: i128 = env.storage().instance().get(&ack).unwrap_or(0i128);

        let gross_amount = (balance * (accrued_total - last_claimed)) / 1_000_000_000_000_000_000i128;
        if gross_amount <= 0 {
            return 0;
        }

        let config: DividendConfig =
            match env.storage().instance().get(&Symbol::new(&env, "config")) {
                Some(c) => c,
                None => return 0,
            };

        let fee_amount = (gross_amount * config.fee_rate as i128) / 10_000i128;
        gross_amount - fee_amount
    }

    pub fn get_accrual_rate(env: Env, token_address: Address) -> Option<AccrualRate> {
        let accruals: Map<Address, AccrualRate> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "accruals"))
            .unwrap_or(Map::new(&env));
        accruals.get(token_address)
    }

    pub fn update_config(env: Env, auth: Address, new_config: DividendConfig) {
        crate::shared_admin::require_admin(&env, &auth);
        Self::check_version(&env);

        // Issue #136: enforce fee rate cap (max 1000 bps = 10%).
        if new_config.fee_rate < 0 {
            panic_with_error!(&env, DividendError::FeeRateNegative);
        }
        if new_config.fee_rate > MAX_FEE_RATE {
            panic_with_error!(&env, DividendError::FeeRateTooHigh);
        }

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "config"), &new_config);
    }

    fn register_token_internal(env: Env, token_address: Address) {
        let mut tokens: Vec<Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "registered_tokens"))
            .unwrap_or(Vec::new(&env));

        if !tokens.iter().any(|t| t == token_address) {
            tokens.push_back(token_address);
            env.storage()
                .instance()
                .set(&Symbol::new(&env, "registered_tokens"), &tokens);
        }
    }

    pub fn calculate_dividend_by_rights(
        env: Env,
        base_amount: i128,
        rights: crate::security::DividendRights,
        unpaid_accrued: i128,
        excess_profit: i128,
    ) -> i128 {
        match rights {
            crate::security::DividendRights::Cumulative => {
                base_amount.saturating_add(unpaid_accrued)
            }
            crate::security::DividendRights::NonCumulative => base_amount,
            crate::security::DividendRights::Participating => {
                base_amount.saturating_add(excess_profit)
            }
            crate::security::DividendRights::NonParticipating => base_amount,
        }
    }

    pub fn claim_accrued_yield(env: Env, claimer: Address, token_address: Address) -> i128 {
        Self::check_version(&env);

        let mut accruals: Map<Address, AccrualRate> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "accruals"))
            .unwrap_or(Map::new(&env));

        let rate = accruals
            .get(token_address.clone())
            .unwrap_or_else(|| panic_with_error!(&env, DividendError::NoAccrualConfigured));

        let now = env.ledger().timestamp();
        let accrued_total = Self::settle_accrual(&env, &rate, now);

        // Persist the settled rate so the next claim resumes from this timestamp.
        let mut updated_rate = rate.clone();
        updated_rate.total_accrued = accrued_total;
        updated_rate.last_accrual_update = now;
        let accrual_currency = updated_rate.currency.clone();
        accruals.set(token_address.clone(), updated_rate);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "accruals"), &accruals);

        let rwa_client = RWATokenClient::new(&env, &token_address);
        let balance = rwa_client.get_balance(&claimer).amount;

        if balance == 0 {
            panic_with_error!(&env, DividendError::NoTokensToClaim);
        }

        let ack = AccrualClaimKey {
            token_address: token_address.clone(),
            claimer: claimer.clone(),
        };
        let last_claimed: i128 = env.storage().instance().get(&ack).unwrap_or(0i128);

        let gross_amount =
            (balance * (accrued_total - last_claimed)) / 1_000_000_000_000_000_000i128;

        if gross_amount <= 0 {
            panic_with_error!(&env, DividendError::NoDividendAvailable);
        }

        let config: DividendConfig = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "config"))
            .unwrap_or_else(|| panic_with_error!(&env, DividendError::ConfigNotFound));

        let fee_amount = (gross_amount * config.fee_rate as i128) / 10_000i128;
        let net_amount = gross_amount - fee_amount;

        let currency_token_address =
            Self::get_currency_token_address(env.clone(), accrual_currency.clone());
        let currency_client = TokenClient::new(&env, &currency_token_address);

        currency_client.transfer(&env.current_contract_address(), &claimer, &net_amount);

        if fee_amount > 0 {
            currency_client.transfer(
                &env.current_contract_address(),
                &config.fee_recipient,
                &fee_amount,
            );
        }

        env.storage().instance().set(&ack, &accrued_total);

        env.events().publish(
            (Symbol::new(&env, "accrued_yield_claimed"), claimer),
            (token_address, net_amount, accrual_currency, now),
        );

        net_amount
    }

    pub fn deactivate_distribution(env: Env, auth: Address, distribution_id: u64) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        // Issue #130: update via map instead of rebuilding a Vec.
        let mut distributions: Map<u64, DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Map::new(&env));

        if let Some(mut dist) = distributions.get(distribution_id) {
            dist.is_active = false;
            distributions.set(distribution_id, dist);
            env.storage()
                .instance()
                .set(&Symbol::new(&env, "distributions"), &distributions);
        }
    }

    // ── Issue #139: emergency freeze & governance-gated unfreeze ─────────────

    /// Whether an emergency freeze is currently active.  Defaults to `false`.
    pub fn is_distributions_frozen(env: Env) -> bool {
        Self::is_frozen(&env)
    }

    fn is_frozen(env: &Env) -> bool {
        env.storage()
            .instance()
            .get(&Symbol::new(env, "distributions_frozen"))
            .unwrap_or(false)
    }

    /// Emergency halt.  Stops both new distributions and all claims until a
    /// governance proposal lifts the freeze.  Admin-only.
    pub fn freeze_all_distributions(env: Env, auth: Address) {
        crate::shared_admin::require_admin(&env, &auth);
        Self::check_version(&env);

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "distributions_frozen"), &true);

        env.events().publish(
            (Symbol::new(&env, "distributions_frozen"), auth),
            (env.ledger().timestamp(),),
        );
    }

    /// Configure the governance committee that must approve an unfreeze.
    pub fn init_distribution_governance(
        env: Env,
        auth: Address,
        owners: Vec<Address>,
        threshold: u32,
        timelock_seconds: u64,
    ) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "admin"))
            .unwrap_or_else(|| panic_with_error!(&env, DividendError::NotInitialized));
        if auth != admin {
            panic_with_error!(&env, DividendError::Unauthorized);
        }

        crate::shared_governance::write_governance(
            &env,
            &auth,
            &owners,
            threshold,
            timelock_seconds,
        );
    }

    /// Create an emergency governance proposal authorising an unfreeze.
    pub fn create_unfreeze_proposal(
        env: Env,
        proposer: Address,
        payload_hash: soroban_sdk::BytesN<32>,
    ) -> u64 {
        let proposal_key = Symbol::new(&env, UNFREEZE_PROPOSAL_KEY);
        crate::shared_governance::create_emergency_proposal(
            env,
            proposer,
            proposal_key,
            payload_hash,
        )
    }

    /// Approve a pending governance proposal (must be a committee owner).
    pub fn approve_distribution_proposal(env: Env, approver: Address, proposal_id: u64) {
        crate::shared_governance::approve(env, approver, proposal_id);
    }

    /// Lift an emergency freeze.
    pub fn unfreeze_all_distributions(
        env: Env,
        executor: Address,
        proposal_id: u64,
        payload_hash: soroban_sdk::BytesN<32>,
    ) -> bool {
        if !Self::is_frozen(&env) {
            panic_with_error!(&env, DividendError::InvalidParameters);
        }

        let proposal_key = Symbol::new(&env, UNFREEZE_PROPOSAL_KEY);
        if !crate::shared_governance::can_execute(&env, proposal_id, proposal_key, payload_hash) {
            panic_with_error!(&env, DividendError::InvalidGovernanceProposal);
        }

        crate::shared_governance::execute_mark(env.clone(), executor.clone(), proposal_id);

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "distributions_frozen"), &false);

        env.events().publish(
            (Symbol::new(&env, "distributions_unfrozen"), executor),
            (proposal_id, env.ledger().timestamp()),
        );

        true
    }

    /// Issue #133: returns all distributions matching the given category.
    pub fn get_distributions_by_category(env: Env, category: DistributionCategory) -> Vec<DividendDistribution> {
        let distributions: Vec<DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Vec::new(&env));

        let mut result = Vec::<DividendDistribution>::new(&env);
        for d in distributions.iter() {
            if d.category == category {
                result.push_back(d.clone());
            }
        }
        result
    }

    /// Issue #134: returns the current claim state for a (distribution, claimer) pair.
    pub fn get_claim_state(env: Env, distribution_id: u64, claimer: Address) -> ClaimState {
        let ck = ClaimKey {
            distribution_id,
            claimer,
        };
        env.storage()
            .instance()
            .get(&ck)
            .unwrap_or(ClaimState::Unclaimed)
    }

    /// Issue #131: creates a distribution bound to a recurring cron-like schedule.
    /// The schedule is stored and can be queried with `get_recurring_schedule`.
    pub fn create_recurring_distribution(
        env: Env,
        auth: Address,
        token_address: Address,
        currency: Symbol,
        amount: i128,
        claim_deadline: u64,
        metadata: Map<Symbol, Symbol>,
        category: DistributionCategory,
        schedule: RecurringSchedule,
    ) -> u64 {
        let currency_for_event = currency.clone();
        let distribution_id = Self::create_distribution(
            env.clone(),
            auth.clone(),
            token_address.clone(),
            currency,
            amount,
            claim_deadline,
            metadata,
            category,
        );

        env.storage()
            .instance()
            .set(
                &Symbol::new(&env, "recurring_schedules"),
                &{
                    let mut schedules: Map<u64, RecurringSchedule> = env
                        .storage()
                        .instance()
                        .get(&Symbol::new(&env, "recurring_schedules"))
                        .unwrap_or(Map::new(&env));
                    schedules.set(distribution_id, schedule);
                    schedules
                },
            );

        env.events().publish(
            (Symbol::new(&env, "recurring_distribution_created"), token_address),
            (distribution_id, amount, currency_for_event, env.ledger().timestamp()),
        );

        distribution_id
    }

    /// Issue #131: returns the recurring schedule for a distribution, if any.
    pub fn get_recurring_schedule(env: Env, distribution_id: u64) -> Option<RecurringSchedule> {
        let schedules: Map<u64, RecurringSchedule> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "recurring_schedules"))
            .unwrap_or(Map::new(&env));
        schedules.get(distribution_id)
    }

    /// Issue #131: checks whether a recurring schedule is due to fire now.
    /// A schedule matches when all non-wildcard (-1) fields equal the current
    /// ledger time components, and the min interval has elapsed since last fire.
    pub fn is_schedule_due(env: Env, distribution_id: u64) -> bool {
        let schedule = match Self::get_recurring_schedule(env.clone(), distribution_id) {
            Some(s) => s,
            None => return false,
        };

        let now = env.ledger().timestamp();
        if now - schedule.last_fired_at < schedule.min_interval_seconds {
            return false;
        }

        // Compute time components from the ledger timestamp.
        // Soroban ledger timestamps are Unix seconds; derive UTC components.
        let secs = now as i64;
        let minute = (secs % 3600 / 60) as i64;
        let hour = (secs % 86400 / 3600) as i64;
        // Approximate day_of_month/month/day_of_week from epoch days.
        let days_since_epoch = (secs / 86400) as i64;
        let day_of_month = ((days_since_epoch % 30) + 1) as i64;
        let month = ((days_since_epoch / 30 % 12) + 1) as i64;
        let day_of_week = (days_since_epoch % 7) as i64;

        let matches = (schedule.minute == -1 || schedule.minute == minute)
            && (schedule.hour == -1 || schedule.hour == hour)
            && (schedule.day_of_month == -1 || schedule.day_of_month == day_of_month)
            && (schedule.month == -1 || schedule.month == month)
            && (schedule.day_of_week == -1 || schedule.day_of_week == day_of_week);

        matches
    }

    fn get_currency_token_address(env: Env, currency: Symbol) -> Address {
        let map: Map<Symbol, Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "currency_tokens"))
            .unwrap_or(Map::new(&env));

        map.get(currency)
            .unwrap_or_else(|| panic_with_error!(&env, DividendError::UnsupportedCurrency))
    }

    // ── Issue #125: dividend reinvestment ─────────────────────────────────

    /// Storage key for a user's reinvestment opt-in. Uses a composite key of
    /// (fixed symbol, user address, token address) so each (user, token)
    /// pair is tracked independently.
    fn reinvest_key(env: &Env, user: &Address, token: &Address) -> (Symbol, Address, Address) {
        (Symbol::new(env, "reinvest"), user.clone(), token.clone())
    }

    /// Opt in to automatic dividend reinvestment for `token_address`. The
    /// caller (user) must authorise this transaction. When opted in, future
    /// `claim_dividend` calls will auto-purchase RWA tokens at the current
    /// VWAP instead of paying out in currency.
    pub fn opt_in_reinvestment(env: Env, user: Address, token_address: Address) {
        user.require_auth();

        let key = Self::reinvest_key(&env, &user, &token_address);
        env.storage().instance().set(&key, &true);

        env.events().publish(
            (
                Symbol::new(&env, "reinvestment_opted_in"),
                user,
                token_address,
            ),
            (env.ledger().timestamp(),),
        );
    }

    /// Opt out of dividend reinvestment for `token_address`. Future claims
    /// will pay out in currency as normal.
    pub fn opt_out_reinvestment(env: Env, user: Address, token_address: Address) {
        user.require_auth();

        let key = Self::reinvest_key(&env, &user, &token_address);
        env.storage().instance().set(&key, &false);

        env.events().publish(
            (
                Symbol::new(&env, "reinvestment_opted_out"),
                user,
                token_address,
            ),
            (env.ledger().timestamp(),),
        );
    }

    /// Whether `address` has opted in to dividend reinvestment for `token`.
    pub fn is_reinvesting(env: Env, address: Address, token: Address) -> bool {
        let key = Self::reinvest_key(&env, &address, &token);
        env.storage().instance().get(&key).unwrap_or(false)
    }

    /// Configure the secondary market address used to look up VWAP prices for
    /// dividend reinvestment. Admin-only.
    pub fn set_market_address(env: Env, auth: Address, market_address: Address) {
        crate::shared_admin::require_admin(&env, &auth);
        Self::check_version(&env);

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "market_addr"), &market_address);

        env.events().publish(
            (Symbol::new(&env, "market_address_set"), auth),
            (market_address, env.ledger().timestamp()),
        );
    }
}
