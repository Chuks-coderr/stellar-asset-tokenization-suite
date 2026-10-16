use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, token::TokenClient, Address, Env, Map,
    Symbol, Vec,
};

use crate::rwa_token::RWATokenClient;

const STORAGE_VERSION: u32 = 1;

/// Default minimum claim window (seconds) enforced on new distributions.
/// Guarantees holders at least a 1-hour window to claim (issue #132).
/// Configurable per-deployment via `DividendConfig.min_claim_window`.
pub const DEFAULT_MIN_CLAIM_WINDOW: u64 = 3600;

/// Proposal key used by the governance committee to authorise lifting an
/// emergency freeze (`freeze_all_distributions`). See issue #139.
pub const UNFREEZE_PROPOSAL_KEY: &str = "unfreeze_distributions";

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
}

#[contracttype]
#[derive(Clone)]
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
        env.storage().instance().set(
            &Symbol::new(&env, "distributions"),
            &Vec::<DividendDistribution>::new(&env),
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
            .set(&Symbol::new(&env, "clm_tot"),             &Map::<u64, i128>::new(&env));
        env.storage()
            .instance()
            .set(
                &Symbol::new(&env, "accruals"),
                &Map::<Address, AccrualRate>::new(&env),
            );
        env.storage()
            .instance()
            .set(
                &Symbol::new(&env, "registered_tokens"),
                &Vec::<Address>::new(&env),
            );
        // Issue #139: no emergency freeze is active at deployment time.
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "distributions_frozen"), &false);
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
            .unwrap_or_else(|| { panic_with_error!(&env, DividendError::ConfigNotFound); });

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
            .unwrap_or_else(|| { panic_with_error!(&env, DividendError::ConfigNotFound); });

        // Issue #132 — require a minimum claim window so distributions cannot
        // be created with a deadline holders cannot realistically meet.
        // `saturating_sub` keeps this panic-free even if the ledger timestamp
        // ever moved backwards between the check above and here.
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

        if total_supply == 0 {
            panic_with_error!(&env, DividendError::ZeroTotalSupply);
        }

        let per_token_amount = (amount * 1000000000000000000i128) / total_supply;

        let distribution_count: u64 = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distribution_count"))
            .unwrap_or(0u64);

        let distribution_id = distribution_count + 1;

        let distribution = DividendDistribution {
            distribution_id,
            token_address: token_address.clone(),
            currency: currency.clone(),
            total_amount: amount,
            per_token_amount,
            total_supply,
            claim_deadline,
            created_at: env.ledger().timestamp(),
            is_active: true,
            metadata,
        };

        let mut distributions: Vec<DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Vec::new(&env));

        distributions.push_back(distribution);
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

        // Track the token so portfolio-wide claims can iterate all held assets
        Self::register_token_internal(env.clone(), token_address.clone());

        env.events().publish(
            (Symbol::new(&env, "distribution_created"), token_address),
            (distribution_id, amount, currency, env.ledger().timestamp()),
        );

        distribution_id
    }

    pub fn claim_dividend(env: Env, distribution_id: u64, claimer: Address) -> i128 {
        Self::check_version(&env);

        // Issue #139: claims are disabled while an emergency freeze is active.
        if Self::is_frozen(&env) {
            panic_with_error!(&env, DividendError::DistributionNotActive);
        }

        let distributions: Vec<DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Vec::new(&env));

        let mut found_dist: Option<DividendDistribution> = None;
        for d in distributions.iter() {
            if d.distribution_id == distribution_id {
                found_dist = Some(d.clone());
                break;
            }
        }
        let distribution = found_dist.unwrap_or_else(|| { panic_with_error!(&env, DividendError::DistributionNotFound); });

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

        let claimable_amount = (balance * distribution.per_token_amount) / 1000000000000000000i128;

        if claimable_amount <= 0 {
            panic_with_error!(&env, DividendError::NoDividendAvailable);
        }

        let config: DividendConfig = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "config"))
            .unwrap_or_else(|| { panic_with_error!(&env, DividendError::ConfigNotFound); });

        let fee_amount = (claimable_amount * config.fee_rate as i128) / 10000i128;
        let net_amount = claimable_amount - fee_amount;

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

        let distributions: Vec<DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Vec::new(&env));

        let mut claimed_amounts = Vec::<i128>::new(&env);
        let current_time = env.ledger().timestamp();

        for distribution in distributions.iter() {
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

        // Bound portfolio iteration to a fixed maximum number of tokens
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

        let distributions: Vec<DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Vec::new(&env));

        let mut total: i128 = 0;
        let current_time = env.ledger().timestamp();

        for distribution in distributions.iter() {
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
        let distributions: Vec<DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Vec::new(&env));

        for d in distributions.iter() {
            if d.distribution_id == distribution_id {
                return d.clone();
            }
        }
        panic_with_error!(&env, DividendError::DistributionNotFound)
    }

    pub fn get_active_distributions(env: Env, token_address: Address) -> Vec<DividendDistribution> {
        let distributions: Vec<DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Vec::new(&env));

        let mut active_distributions = Vec::<DividendDistribution>::new(&env);
        let current_time = env.ledger().timestamp();

        for distribution in distributions.iter() {
            if distribution.token_address == token_address
                && distribution.is_active
                && current_time <= distribution.claim_deadline
            {
                active_distributions.push_back(distribution.clone());
            }
        }

        active_distributions
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

        let distributions: Vec<DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Vec::new(&env));

        let mut found_dist: Option<DividendDistribution> = None;
        for d in distributions.iter() {
            if d.distribution_id == distribution_id {
                found_dist = Some(d.clone());
                break;
            }
        }
        let distribution = match found_dist {
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

        let claimable_amount = (balance * distribution.per_token_amount) / 1000000000000000000i128;

        let config: DividendConfig = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "config"))
            .unwrap_or_else(|| { panic_with_error!(&env, DividendError::ConfigNotFound); });

        let fee_amount = (claimable_amount * config.fee_rate as i128) / 10000i128;
        claimable_amount - fee_amount
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
            .unwrap_or_else(|| { panic_with_error!(&env, DividendError::ConfigNotFound); });

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

        let gross_amount = (balance * (accrued_total - last_claimed)) / 1000000000000000000i128;
        if gross_amount <= 0 {
            return 0;
        }

        let config: DividendConfig = match env.storage().instance().get(&Symbol::new(&env, "config")) {
            Some(c) => c,
            None => return 0,
        };

        let fee_amount = (gross_amount * config.fee_rate as i128) / 10000i128;
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
            crate::security::DividendRights::NonCumulative => {
                base_amount
            }
            crate::security::DividendRights::Participating => {
                base_amount.saturating_add(excess_profit)
            }
            crate::security::DividendRights::NonParticipating => {
                base_amount
            }
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

        // Persist the settled rate so the next claim resumes from this timestamp
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

        let gross_amount = (balance * (accrued_total - last_claimed)) / 1000000000000000000i128;

        if gross_amount <= 0 {
            panic_with_error!(&env, DividendError::NoDividendAvailable);
        }

        let config: DividendConfig = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "config"))
            .unwrap_or_else(|| { panic_with_error!(&env, DividendError::ConfigNotFound); });

        let fee_amount = (gross_amount * config.fee_rate as i128) / 10000i128;
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

        let distributions: Vec<DividendDistribution> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "distributions"))
            .unwrap_or(Vec::new(&env));

        let mut found = false;
        let mut new_distributions = Vec::<DividendDistribution>::new(&env);

        for distribution in distributions.iter() {
            if distribution.distribution_id == distribution_id {
                let mut u = distribution.clone();
                u.is_active = false;
                new_distributions.push_back(u);
                found = true;
            } else {
                new_distributions.push_back(distribution.clone());
            }
        }

        if found {
            env.storage()
                .instance()
                .set(&Symbol::new(&env, "distributions"), &new_distributions);
        }
    }

    // ── Issue #139: emergency freeze & governance-gated unfreeze ──────────

    /// Whether an emergency freeze is currently active. Defaults to `false`.
    pub fn is_distributions_frozen(env: Env) -> bool {
        Self::is_frozen(&env)
    }

    fn is_frozen(env: &Env) -> bool {
        env.storage()
            .instance()
            .get(&Symbol::new(env, "distributions_frozen"))
            .unwrap_or(false)
    }

    /// Emergency halt. Stops both new distributions and all claims until a
    /// governance proposal lifts the freeze. Admin-only.
    ///
    /// Existing distributions and already-accrued balances are left untouched
    /// and become claimable again once the freeze is lifted.
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
    ///
    /// Mirrors the shared governance module: `owners` are the eligible
    /// approvers, `threshold` the number of unique approvals required and
    /// `timelock_seconds` the delay before a proposal becomes executable.
    pub fn init_distribution_governance(
        env: Env,
        auth: Address,
        owners: Vec<Address>,
        threshold: u32,
        timelock_seconds: u64,
    ) {
        // Verify the caller is the contract admin *without* calling
        // `require_auth` here: `write_governance` performs the single auth
        // requirement for this frame.
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
    ///
    /// Once it has been approved by the committee and passes its review
    /// window, call `unfreeze_all_distributions` with the returned id and the
    /// same `payload_hash`. Exposed as `create_unfreeze_proposal` because
    /// Soroban caps exported function names at 32 characters.
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
    ///
    /// This deliberately does **not** accept a plain admin call: the caller
    /// must supply an approved and executable governance proposal. A direct
    /// admin call (or one pointing at a non-approved proposal) is rejected.
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
        if !crate::shared_governance::can_execute(
            &env,
            proposal_id,
            proposal_key,
            payload_hash,
        ) {
            panic_with_error!(&env, DividendError::InvalidGovernanceProposal);
        }

        // Mark the proposal executed (also re-checks vetos / auth).
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

    fn get_currency_token_address(env: Env, currency: Symbol) -> Address {
        let map: Map<Symbol, Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "currency_tokens"))
            .unwrap_or(Map::new(&env));

        map.get(currency)
            .unwrap_or_else(|| { panic_with_error!(&env, DividendError::UnsupportedCurrency); })
    }
}
