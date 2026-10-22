use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, Address, Env, Map,
    Symbol, Vec,
};

const STORAGE_VERSION: u32 = 1;

/// The number of seconds in a UTC day.
const SECONDS_PER_DAY: u64 = 86_400;

/// How long before its expiry a KYC record enters the warning state
/// (Issue #111).
const KYC_WARNING_WINDOW_SECONDS: u64 = 7 * 24 * 60 * 60;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum ComplianceError {
    Unauthorized = 1,
    UserNotFound = 2,
    KYCNotVerified = 3,
    Blacklisted = 4,
    InvalidJurisdiction = 5,
    AccreditationRequired = 6,
    TransferLimitExceeded = 7,
    AlreadyInitialized = 8,
    NotInitialized = 9,
    StorageOutdated = 10,
    RegistryNotInitialized = 11,
    AlreadyAtLatestVersion = 12,
    DelegateNotFound = 13,
    InvalidDateRange = 14,
    ReportTooLarge = 15,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KYCStatus {
    pub is_verified: bool,
    pub verification_level: u32,
    pub expiry_date: u64,
    pub jurisdiction: Symbol,
    pub is_accredited: bool,
    pub risk_score: u32,
    pub aml_flags: Vec<Symbol>,
}

/// The effective KYC state of a participant (Issue #111).
///
/// - `Active`  — verified and still comfortably within its validity period.
/// - `Warning` — verified, but the record expires within the warning window.
/// - `Expired` — not verified, or the expiry date has passed.
/// - `Revoked` — the address is on the blacklist.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KycState {
    Active = 0,
    Warning = 1,
    Expired = 2,
    Revoked = 3,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferLimits {
    pub daily_limit: i128,
    pub monthly_limit: i128,
    pub annual_limit: i128,
    pub remaining_daily: i128,
    pub remaining_monthly: i128,
    pub remaining_annual: i128,
    pub last_reset_daily: u64,
    pub last_reset_monthly: u64,
    pub last_reset_annual: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComplianceRule {
    pub rule_id: Symbol,
    pub name: Symbol,
    pub description: Symbol,
    pub is_active: bool,
    pub jurisdictions: Vec<Symbol>,
    pub min_verification_level: u32,
    pub requires_accreditation: bool,
    pub max_amount: i128,
}

/// Issue #122: An external compliance check submitted by a delegated third-party
/// integrator. These checks supplement — but do NOT replace — on-chain KYC and
/// are never consulted by `check_compliance`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegatedCheck {
    pub delegate: Address,
    pub user: Address,
    pub result: bool,
    pub evidence_hash: Symbol,
    pub timestamp: u64,
}

/// Issue #123: Per-asset-class risk score thresholds. When no entry exists for a
/// given asset type the default (max_risk_score = 8, require_aml_clearance_above
/// = 8) is used.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RiskThresholds {
    pub max_risk_score: u32,
    pub require_aml_clearance_above: u32,
}

/// Issue #124: A single entry in the compliance audit log used to build
/// regulatory reports.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReportEntry {
    pub timestamp: u64,
    pub entry_type: Symbol,
    pub address: Address,
    pub details: Symbol,
}

/// Issue #124: A structured compliance report covering a bounded date range.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComplianceReport {
    pub report_type: Symbol,
    pub start: u64,
    pub end: u64,
    pub entries: Vec<ReportEntry>,
}

// ... (around line 58)
#[contract]
pub struct ComplianceRegistry;

/// The UTC calendar year and month (1-12) a timestamp falls in.
///
/// Hinnant's `civil_from_days` algorithm: days since the Unix epoch are
/// converted to a civil date. Used by the calendar-aligned transfer-limit
/// resets (Issue #113).
fn utc_calendar(timestamp: u64) -> (i64, u32) {
    let z = (timestamp / SECONDS_PER_DAY) as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;                                     // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);                // [0, 365]
    let mp = (5 * doy + 2) / 153;                                    // [0, 11]
    let m = if mp < 10 { mp + 3 } else { mp - 9 };                    // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32)
}

/// The UTC day a timestamp falls in (Issue #113).
fn utc_day(timestamp: u64) -> u64 {
    timestamp / SECONDS_PER_DAY
}

/// The UTC calendar month a timestamp falls in, as `year * 12 + (month - 1)`
/// (Issue #113).
fn utc_month_index(timestamp: u64) -> u32 {
    let (year, month) = utc_calendar(timestamp);
    (year as u32) * 12 + (month - 1)
}

/// The UTC calendar year a timestamp falls in (Issue #113).
fn utc_year_index(timestamp: u64) -> u32 {
    utc_calendar(timestamp).0 as u32
}

#[contractimpl]

impl ComplianceRegistry {
    // ...

    pub fn initialize(
        env: Env,
        auth: Address,
        admin: Address,
        kyc_required: bool,
        transfer_restrictions: bool,
    ) {
        // `write_admin` performs the single auth check for this frame.
        crate::shared_admin::write_admin(&env, &auth, &admin);
        if env
            .storage()
            .instance()
            .has(&Symbol::new(&env, "initialized"))
        {
            panic_with_error!(&env, ComplianceError::AlreadyInitialized);
        }

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "version"), &STORAGE_VERSION);

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "admin"), &admin);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "kyc_required"), &kyc_required);
        env.storage().instance().set(
            &Symbol::new(&env, "transfer_restrictions"),
            &transfer_restrictions,
        );
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "initialized"), &true);

        // Seed the three default SEC compliance rules (rule_144, reg_d, reg_s).
        let mut rules = Vec::<ComplianceRule>::new(&env);
        rules.push_back(ComplianceRule {
            rule_id: Symbol::new(&env, "rule_144"),
            name: Symbol::new(&env, "Rule144"),
            description: Symbol::new(&env, "restricted_securities_resale"),
            is_active: true,
            jurisdictions: Vec::new(&env),
            min_verification_level: 1,
            requires_accreditation: false,
            max_amount: 1_000_000,
        });
        rules.push_back(ComplianceRule {
            rule_id: Symbol::new(&env, "reg_d"),
            name: Symbol::new(&env, "RegD"),
            description: Symbol::new(&env, "private_placement_exemption"),
            is_active: true,
            jurisdictions: Vec::new(&env),
            min_verification_level: 2,
            requires_accreditation: true,
            max_amount: 5_000_000,
        });
        rules.push_back(ComplianceRule {
            rule_id: Symbol::new(&env, "reg_s"),
            name: Symbol::new(&env, "RegS"),
            description: Symbol::new(&env, "non_us_offering"),
            is_active: true,
            jurisdictions: Vec::new(&env),
            min_verification_level: 1,
            requires_accreditation: false,
            max_amount: 10_000_000,
        });
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "compliance_rules"), &rules);

        env.events().publish(
            (Symbol::new(&env, "registry_initialized"), admin),
            (kyc_required, transfer_restrictions),
        );
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
            panic_with_error!(env, ComplianceError::StorageOutdated);
        }
    }

    pub fn migrate(env: Env, auth: Address) {
        crate::shared_admin::require_admin(&env, &auth);

        let ver = Self::read_version(&env);
        if ver >= STORAGE_VERSION {
            panic_with_error!(&env, ComplianceError::AlreadyAtLatestVersion);
        }

        let mut current = ver;
        while current < STORAGE_VERSION {
            current += 1;
        }

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "version"), &STORAGE_VERSION);
    }

    pub fn update_kyc_status(env: Env, auth: Address, user: Address, kyc_status: KYCStatus) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        env.storage().instance().set(&user, &kyc_status);

        // Issue #124: log the KYC verification for regulatory reporting.
        if kyc_status.is_verified {
            Self::log_compliance_event(
                &env,
                Symbol::new(&env, "kyc_verified"),
                user.clone(),
                Symbol::new(&env, "verified"),
            );
        }

        env.events().publish(
            (Symbol::new(&env, "kyc_updated"), user, auth),
            (
                kyc_status.is_verified,
                kyc_status.verification_level,
                kyc_status.jurisdiction,
                kyc_status.risk_score,
            ),
        );
    }

    /// Whether participants must be KYC-verified before they can trade.
    ///
    /// The secondary market consults this before gating an order so a registry
    /// configured with `kyc_required == false` keeps working without every
    /// participant being verified.
    pub fn is_kyc_required(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "kyc_required"))
            .unwrap_or(true)
    }

    pub fn get_kyc_status(env: Env, user: Address) -> KYCStatus {
        let mut kyc: KYCStatus = env.storage().instance().get(&user).unwrap_or(KYCStatus {
            is_verified: false,
            verification_level: 0,
            expiry_date: 0,
            jurisdiction: Symbol::new(&env, "UNKNOWN"),
            is_accredited: false,
            risk_score: 5,
            aml_flags: Vec::new(&env),
        });

        // Issue #111 — report the *effective* state: a record that has
        // expired or been revoked is not verified any more, even though the
        // stored record still says it is.
        match Self::get_effective_kyc_state(env.clone(), user.clone()) {
            KycState::Expired | KycState::Revoked => kyc.is_verified = false,
            _ => {}
        }

        kyc
    }

    /// The effective KYC state of `user` (Issue #111).
    ///
    /// - `Revoked` — the address is on the blacklist.
    /// - `Expired` — the record is not verified, or its expiry date has
    ///   passed (an expiry date of 0 means "never set", which is expired).
    /// - `Warning` — the record expires within the warning window.
    /// - `Active`  — the record is verified and still comfortably valid.
    pub fn get_effective_kyc_state(env: Env, user: Address) -> KycState {
        let blacklist: Vec<Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "blacklist"))
            .unwrap_or(Vec::new(&env));

        if blacklist.iter().any(|a| a.clone() == user) {
            return KycState::Revoked;
        }

        let kyc: KYCStatus = env.storage().instance().get(&user).unwrap_or(KYCStatus {
            is_verified: false,
            verification_level: 0,
            expiry_date: 0,
            jurisdiction: Symbol::new(&env, "UNKNOWN"),
            is_accredited: false,
            risk_score: 5,
            aml_flags: Vec::new(&env),
        });

        if !kyc.is_verified {
            return KycState::Expired;
        }

        let now = env.ledger().timestamp();
        if kyc.expiry_date == 0 || kyc.expiry_date < now {
            return KycState::Expired;
        }
        if kyc.expiry_date - now <= KYC_WARNING_WINDOW_SECONDS {
            return KycState::Warning;
        }

        KycState::Active
    }

    pub fn add_to_blacklist(env: Env, auth: Address, address: Address, reason: Symbol) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let mut blacklist: Vec<Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "blacklist"))
            .unwrap_or(Vec::new(&env));

        blacklist.push_back(address.clone());
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "blacklist"), &blacklist);

        // Issue #124: log the blacklist change for regulatory reporting.
        Self::log_compliance_event(
            &env,
            Symbol::new(&env, "blacklisted"),
            address.clone(),
            reason.clone(),
        );

        env.events()
            .publish((Symbol::new(&env, "blacklisted"), address, auth), reason);
    }

    pub fn remove_from_blacklist(env: Env, auth: Address, address: Address) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let blacklist: Vec<Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "blacklist"))
            .unwrap_or(Vec::new(&env));

        let mut new_blacklist = Vec::<Address>::new(&env);
        let mut found = false;
        for addr in blacklist.iter() {
            if addr.clone() == address {
                found = true;
            } else {
                new_blacklist.push_back(addr.clone());
            }
        }

        if found {
            env.storage()
                .instance()
                .set(&Symbol::new(&env, "blacklist"), &new_blacklist);

            // Issue #124: log the un-blacklist change for regulatory reporting.
            Self::log_compliance_event(
                &env,
                Symbol::new(&env, "unblacklisted"),
                address.clone(),
                Symbol::new(&env, "removed"),
            );

            env.events().publish(
                (Symbol::new(&env, "unblacklisted"), address, auth),
                Symbol::new(&env, "removed"),
            );
        }
    }

    pub fn add_to_whitelist(env: Env, auth: Address, address: Address) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let mut whitelist: Vec<Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "whitelist"))
            .unwrap_or(Vec::new(&env));

        whitelist.push_back(address.clone());
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "whitelist"), &whitelist);

        env.events().publish(
            (Symbol::new(&env, "whitelisted"), address, auth),
            Symbol::new(&env, "added"),
        );
    }

    pub fn remove_from_whitelist(env: Env, auth: Address, address: Address) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let whitelist: Vec<Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "whitelist"))
            .unwrap_or(Vec::new(&env));

        let mut new_whitelist = Vec::<Address>::new(&env);
        let mut found = false;
        for addr in whitelist.iter() {
            if addr.clone() == address {
                found = true;
            } else {
                new_whitelist.push_back(addr.clone());
            }
        }

        if found {
            env.storage()
                .instance()
                .set(&Symbol::new(&env, "whitelist"), &new_whitelist);
            env.events().publish(
                (Symbol::new(&env, "unwhitelisted"), address, auth),
                Symbol::new(&env, "removed"),
            );
        }
    }

    pub fn check_compliance(env: Env, from: Address, to: Address, amount: i128) -> bool {
        let result = Self::check_compliance_internal(env.clone(), from.clone(), to.clone(), amount);

        env.events().publish(
            (Symbol::new(&env, "compliance_check"), from.clone(), to.clone()),
            (amount, result),
        );

        // Issue #124: log the compliance check for regulatory reporting.
        Self::log_compliance_event(
            &env,
            Symbol::new(&env, "compliance_check"),
            from,
            if result {
                Symbol::new(&env, "passed")
            } else {
                Symbol::new(&env, "failed")
            },
        );

        result
    }

    /// Issue #123: Asset-class-aware compliance check. Uses the risk thresholds
    /// configured for `asset_type` (falling back to the default of 8) instead of
    /// the hardcoded threshold used by `check_compliance`.
    pub fn check_compliance_with_asset(
        env: Env,
        from: Address,
        to: Address,
        amount: i128,
        asset_type: Symbol,
    ) -> bool {
        let thresholds = Self::get_risk_thresholds(env.clone(), asset_type);
        let result = Self::check_compliance_with_threshold(
            env.clone(),
            from.clone(),
            to.clone(),
            amount,
            thresholds,
        );

        env.events().publish(
            (Symbol::new(&env, "compliance_check"), from.clone(), to.clone()),
            (amount, result),
        );

        // Issue #124: log the compliance check for regulatory reporting.
        Self::log_compliance_event(
            &env,
            Symbol::new(&env, "compliance_check"),
            from,
            if result {
                Symbol::new(&env, "passed")
            } else {
                Symbol::new(&env, "failed")
            },
        );

        result
    }

    fn check_compliance_internal(env: Env, from: Address, to: Address, amount: i128) -> bool {
        // Default threshold of 8 — keeps existing `check_compliance` behaviour.
        Self::check_compliance_with_threshold(
            env,
            from,
            to,
            amount,
            RiskThresholds {
                max_risk_score: 8,
                require_aml_clearance_above: 8,
            },
        )
    }

    fn check_compliance_with_threshold(
        env: Env,
        from: Address,
        to: Address,
        amount: i128,
        thresholds: RiskThresholds,
    ) -> bool {
        let kyc_required: bool = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "kyc_required"))
            .unwrap_or(true);

        // 1. Blacklist check — always enforced, no exceptions
        let blacklist: Vec<Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "blacklist"))
            .unwrap_or(Vec::new(&env));

        for addr in blacklist.iter() {
            if addr.clone() == from || addr.clone() == to {
                return false;
            }
        }

        // 2. KYC / whitelist checks
        if kyc_required {
            let whitelist: Vec<Address> = env
                .storage()
                .instance()
                .get(&Symbol::new(&env, "whitelist"))
                .unwrap_or(Vec::new(&env));

            let from_whitelisted = whitelist.iter().any(|a| a.clone() == from);
            let to_whitelisted = whitelist.iter().any(|a| a.clone() == to);

            // Whitelist only bypasses the KYC verification/expiry check —
            // AML flags and risk score are still enforced for whitelisted addresses.
            let current_time = env.ledger().timestamp();

            if !from_whitelisted || !to_whitelisted {
                let from_kyc = Self::get_kyc_status(env.clone(), from.clone());
                let to_kyc = Self::get_kyc_status(env.clone(), to.clone());

                if !from_kyc.is_verified || !to_kyc.is_verified {
                    return false;
                }

                // Reject expired KYC — expiry_date 0 means never set, treat as expired
                if from_kyc.expiry_date == 0 || from_kyc.expiry_date < current_time {
                    return false;
                }
                if to_kyc.expiry_date == 0 || to_kyc.expiry_date < current_time {
                    return false;
                }
            }

            // 3. AML flag check — enforced for all participants including whitelisted
            let from_kyc = Self::get_kyc_status(env.clone(), from.clone());
            let to_kyc = Self::get_kyc_status(env.clone(), to.clone());

            if from_kyc.aml_flags.len() > 0 || to_kyc.aml_flags.len() > 0 {
                return false;
            }

            // 4. Risk score check — reject high-risk participants using the
            //    supplied (asset-specific or default) thresholds.
            if from_kyc.risk_score >= thresholds.max_risk_score
                || to_kyc.risk_score >= thresholds.max_risk_score
            {
                return false;
            }
        }

        // 5. Transfer limits — applied to sender; receiver limit check is read-only
        let transfer_restrictions: bool = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "transfer_restrictions"))
            .unwrap_or(false);

        if transfer_restrictions {
            if !Self::check_transfer_limits(env.clone(), from.clone(), amount) {
                return false;
            }
            // Check receiver has capacity without consuming their limit
            if !Self::check_transfer_limits_readonly(env.clone(), to.clone(), amount) {
                return false;
            }
        }

        // 6. Regulatory rules — always enforced, independent of transfer_restrictions flag
        if !Self::evaluate_regulatory_rules(env.clone(), from.clone(), to.clone(), amount) {
            return false;
        }

        true
    }

    fn evaluate_regulatory_rules(env: Env, from: Address, to: Address, amount: i128) -> bool {
        let rules: Vec<ComplianceRule> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "compliance_rules"))
            .unwrap_or(Vec::new(&env));

        let from_kyc = Self::get_kyc_status(env.clone(), from);
        let to_kyc = Self::get_kyc_status(env.clone(), to);
        let from_jurisdiction = from_kyc.jurisdiction.clone();
        let to_jurisdiction = to_kyc.jurisdiction.clone();

        for rule in rules.iter() {
            if !rule.is_active {
                continue;
            }

            if !Self::rule_applies_to_transfer(
                &env,
                &rule,
                from_jurisdiction.clone(),
                to_jurisdiction.clone(),
            ) {
                continue;
            }

            if from_kyc.verification_level < rule.min_verification_level
                || to_kyc.verification_level < rule.min_verification_level
            {
                return false;
            }

            if rule.requires_accreditation && (!from_kyc.is_accredited || !to_kyc.is_accredited) {
                return false;
            }

            if amount > rule.max_amount {
                return false;
            }
        }

        true
    }

    fn rule_applies_to_transfer(
        env: &Env,
        rule: &ComplianceRule,
        from_jurisdiction: Symbol,
        to_jurisdiction: Symbol,
    ) -> bool {
        for j in rule.jurisdictions.iter() {
            if j == from_jurisdiction || j == to_jurisdiction {
                return true;
            }
        }
        let unknown = Symbol::new(env, "UNKNOWN");
        if from_jurisdiction == unknown || to_jurisdiction == unknown {
            return rule.jurisdictions.len() > 0;
        }
        false
    }

    pub fn check_outbound_participant(env: Env, participant: Address, amount: i128) -> bool {
        let result =
            Self::check_outbound_participant_internal(env.clone(), participant.clone(), amount);

        env.events().publish(
            (Symbol::new(&env, "outbound_compliance_check"), participant),
            (amount, result),
        );

        result
    }

    fn check_outbound_participant_internal(env: Env, participant: Address, amount: i128) -> bool {
        let blacklist: Vec<Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "blacklist"))
            .unwrap_or(Vec::new(&env));

        for addr in blacklist.iter() {
            if addr.clone() == participant {
                return false;
            }
        }

        let kyc_required: bool = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "kyc_required"))
            .unwrap_or(true);

        if kyc_required {
            let kyc = Self::get_kyc_status(env.clone(), participant.clone());

            if !kyc.is_verified {
                return false;
            }

            // expiry_date 0 means never set — treat as expired
            let current_time = env.ledger().timestamp();
            if kyc.expiry_date == 0 || kyc.expiry_date < current_time {
                return false;
            }

            // AML flags and high risk score block outbound operations
            if kyc.aml_flags.len() > 0 {
                return false;
            }
            if kyc.risk_score >= 8 {
                return false;
            }
        }

        let transfer_restrictions: bool = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "transfer_restrictions"))
            .unwrap_or(false);

        if transfer_restrictions
            && !Self::check_transfer_limits(env.clone(), participant.clone(), amount)
        {
            return false;
        }

        true
    }

    pub fn check_transfer_limits(env: Env, user: Address, amount: i128) -> bool {
        let result = Self::check_transfer_limits_internal(env.clone(), user.clone(), amount);

        env.events().publish(
            (Symbol::new(&env, "transfer_limit_check"), user),
            (amount, result),
        );

        result
    }

    fn check_transfer_limits_internal(env: Env, user: Address, amount: i128) -> bool {
        let map_key = Symbol::new(&env, "xfer_lim");
        let mut map: Map<Address, TransferLimits> = env
            .storage()
            .instance()
            .get(&map_key)
            .unwrap_or(Map::new(&env));

        let limits: TransferLimits = map.get(user.clone()).unwrap_or(TransferLimits {
            daily_limit: 10000,
            monthly_limit: 100000,
            annual_limit: 1000000,
            remaining_daily: 10000,
            remaining_monthly: 100000,
            remaining_annual: 1000000,
            last_reset_daily: 0,
            last_reset_monthly: 0,
            last_reset_annual: 0,
        });

        let current_time = env.ledger().timestamp();
        let mut limits = Self::effective_limits(&env, limits, &user, current_time);

        if amount > limits.remaining_daily
            || amount > limits.remaining_monthly
            || amount > limits.remaining_annual
        {
            return false;
        }

        limits.remaining_daily -= amount;
        limits.remaining_monthly -= amount;
        limits.remaining_annual -= amount;

        map.set(user, limits);
        env.storage().instance().set(&map_key, &map);
        true
    }

    /// Read-only variant of check_transfer_limits — verifies capacity without consuming limits.
    /// Used to validate the receiver side of a transfer without side effects.
    fn check_transfer_limits_readonly(env: Env, user: Address, amount: i128) -> bool {
        let map_key = Symbol::new(&env, "xfer_lim");
        let map: Map<Address, TransferLimits> = env
            .storage()
            .instance()
            .get(&map_key)
            .unwrap_or(Map::new(&env));

        let limits: TransferLimits = map.get(user.clone()).unwrap_or(TransferLimits {
            daily_limit: 10000,
            monthly_limit: 100000,
            annual_limit: 1000000,
            remaining_daily: 10000,
            remaining_monthly: 100000,
            remaining_annual: 1000000,
            last_reset_daily: 0,
            last_reset_monthly: 0,
            last_reset_annual: 0,
        });

        let current_time = env.ledger().timestamp();
        let limits = Self::effective_limits(&env, limits, &user, current_time);

        amount <= limits.remaining_daily && amount <= limits.remaining_monthly && amount <= limits.remaining_annual
    }

    /// Applies the calendar-aligned resets to `limits` (Issue #113).
    ///
    /// The daily window rolls over at 00:00 UTC, the monthly one on the 1st
    /// and the annual one on 1 January. The stored `last_reset_*` values stay
    /// timestamps and are compared by calendar period, so rows written before
    /// this change keep working without a migration.
    fn apply_calendar_resets(limits: &mut TransferLimits, current_time: u64) {
        if utc_day(current_time) != utc_day(limits.last_reset_daily) {
            limits.remaining_daily = limits.daily_limit;
            limits.last_reset_daily = current_time;
        }

        if utc_month_index(current_time) != utc_month_index(limits.last_reset_monthly) {
            limits.remaining_monthly = limits.monthly_limit;
            limits.last_reset_monthly = current_time;
        }

        if utc_year_index(current_time) != utc_year_index(limits.last_reset_annual) {
            limits.remaining_annual = limits.annual_limit;
            limits.last_reset_annual = current_time;
        }
    }

    /// The limits that apply to `user` at `current_time` (Issues #111, #113).
    ///
    /// The calendar-aligned resets are applied, and a participant whose KYC
    /// is inside the warning window is capped at half of their normal limits.
    fn effective_limits(
        env: &Env,
        mut limits: TransferLimits,
        user: &Address,
        current_time: u64,
    ) -> TransferLimits {
        Self::apply_calendar_resets(&mut limits, current_time);

        // Issue #111 — a participant whose KYC is inside the warning window
        // may still transfer, but only up to half of their normal limit.
        if Self::get_effective_kyc_state(env.clone(), user.clone()) == KycState::Warning {
            limits.remaining_daily = limits.remaining_daily.min(limits.daily_limit / 2);
            limits.remaining_monthly = limits.remaining_monthly.min(limits.monthly_limit / 2);
            limits.remaining_annual = limits.remaining_annual.min(limits.annual_limit / 2);
        }

        limits
    }

    pub fn set_transfer_limits(env: Env, auth: Address, user: Address, limits: TransferLimits) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let map_key = Symbol::new(&env, "xfer_lim");
        let mut map: Map<Address, TransferLimits> = env
            .storage()
            .instance()
            .get(&map_key)
            .unwrap_or(Map::new(&env));
        map.set(user.clone(), limits.clone());
        env.storage().instance().set(&map_key, &map);

        env.events().publish(
            (Symbol::new(&env, "transfer_limits_set"), user, auth),
            (
                limits.daily_limit,
                limits.monthly_limit,
                limits.annual_limit,
            ),
        );
    }

    pub fn get_compliance_rules(env: Env) -> Vec<ComplianceRule> {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "compliance_rules"))
            .unwrap_or(Vec::new(&env))
    }

    pub fn update_compliance_rule(env: Env, auth: Address, rule: ComplianceRule) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let rules: Vec<ComplianceRule> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "compliance_rules"))
            .unwrap_or(Vec::new(&env));

        let mut found = false;
        let mut new_rules = Vec::<ComplianceRule>::new(&env);
        for existing_rule in rules.iter() {
            if existing_rule.rule_id == rule.rule_id {
                new_rules.push_back(rule.clone());
                found = true;
            } else {
                new_rules.push_back(existing_rule.clone());
            }
        }

        if found {
            env.storage()
                .instance()
                .set(&Symbol::new(&env, "compliance_rules"), &new_rules);
        }

        env.events().publish(
            (Symbol::new(&env, "compliance_rule_updated"), auth),
            (rule.rule_id, rule.name, rule.is_active),
        );
    }

    // ── Issue #124: compliance event logging (private helper) ──────────────────

    /// Append a single entry to the compliance audit log stored under
    /// `compliance_log`. This log is later filtered by `generate_report` to
    /// produce regulatory reports.
    fn log_compliance_event(env: &Env, entry_type: Symbol, address: Address, details: Symbol) {
        let mut log: Vec<ReportEntry> = env
            .storage()
            .instance()
            .get(&Symbol::new(env, "compliance_log"))
            .unwrap_or(Vec::new(env));

        log.push_back(ReportEntry {
            timestamp: env.ledger().timestamp(),
            entry_type,
            address,
            details,
        });

        env.storage()
            .instance()
            .set(&Symbol::new(env, "compliance_log"), &log);
    }

    // ── Issue #122: delegated compliance verification ───────────────────────────

    /// Register a third-party delegate authorised to submit external compliance
    /// checks for `user`. Admin-only.
    pub fn delegate_compliance_check(env: Env, auth: Address, delegate: Address, user: Address) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let mut delegates: Vec<Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "delegates"))
            .unwrap_or(Vec::new(&env));

        // Avoid duplicate registrations.
        let already = delegates.iter().any(|a| a.clone() == delegate);
        if !already {
            delegates.push_back(delegate.clone());
            env.storage()
                .instance()
                .set(&Symbol::new(&env, "delegates"), &delegates);
        }

        env.events().publish(
            (Symbol::new(&env, "delegate_registered"), delegate, user),
            auth,
        );
    }

    /// Remove a previously-registered delegate. Admin-only.
    pub fn remove_delegate(env: Env, auth: Address, delegate: Address) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let delegates: Vec<Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "delegates"))
            .unwrap_or(Vec::new(&env));

        let mut new_delegates = Vec::<Address>::new(&env);
        let mut found = false;
        for addr in delegates.iter() {
            if addr.clone() == delegate {
                found = true;
            } else {
                new_delegates.push_back(addr.clone());
            }
        }

        if !found {
            panic_with_error!(&env, ComplianceError::DelegateNotFound);
        }

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "delegates"), &new_delegates);

        env.events()
            .publish((Symbol::new(&env, "delegate_removed"), delegate), auth);
    }

    /// List all currently-registered delegates.
    pub fn list_delegates(env: Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "delegates"))
            .unwrap_or(Vec::new(&env))
    }

    /// Submit an external compliance check. The caller must be a registered
    /// delegate. External checks are stored separately from on-chain KYC and
    /// are NOT consulted by `check_compliance`.
    pub fn submit_external_check(
        env: Env,
        delegate: Address,
        user: Address,
        result: bool,
        evidence_hash: Symbol,
    ) {
        delegate.require_auth();

        Self::check_version(&env);

        let delegates: Vec<Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "delegates"))
            .unwrap_or(Vec::new(&env));

        let is_delegate = delegates.iter().any(|a| a.clone() == delegate);
        if !is_delegate {
            panic_with_error!(&env, ComplianceError::DelegateNotFound);
        }

        let timestamp = env.ledger().timestamp();

        let mut checks: Vec<DelegatedCheck> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "external_checks"))
            .unwrap_or(Vec::new(&env));

        checks.push_back(DelegatedCheck {
            delegate: delegate.clone(),
            user: user.clone(),
            result,
            evidence_hash: evidence_hash.clone(),
            timestamp,
        });

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "external_checks"), &checks);

        env.events().publish(
            (Symbol::new(&env, "external_check_submitted"), delegate, user),
            (result, evidence_hash, timestamp),
        );
    }

    /// Return all external checks submitted for `user`.
    pub fn get_external_checks(env: Env, user: Address) -> Vec<DelegatedCheck> {
        let checks: Vec<DelegatedCheck> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "external_checks"))
            .unwrap_or(Vec::new(&env));

        let mut result = Vec::<DelegatedCheck>::new(&env);
        for check in checks.iter() {
            if check.user == user {
                result.push_back(check.clone());
            }
        }
        result
    }

    // ── Issue #123: configurable risk score thresholds ────────────────────────

    /// Set risk score thresholds for a specific asset type. Admin-only.
    pub fn set_risk_thresholds(env: Env, auth: Address, asset_type: Symbol, thresholds: RiskThresholds) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let map_key = Symbol::new(&env, "risk_thresholds");
        let mut map: Map<Symbol, RiskThresholds> = env
            .storage()
            .instance()
            .get(&map_key)
            .unwrap_or(Map::new(&env));

        map.set(asset_type.clone(), thresholds.clone());
        env.storage().instance().set(&map_key, &map);

        env.events().publish(
            (Symbol::new(&env, "risk_thresholds_set"), asset_type),
            (thresholds.max_risk_score, thresholds.require_aml_clearance_above),
        );
    }

    /// Get risk thresholds for `asset_type`. Returns the default
    /// (max_risk_score = 8, require_aml_clearance_above = 8) when no thresholds
    /// have been configured for the asset type.
    pub fn get_risk_thresholds(env: Env, asset_type: Symbol) -> RiskThresholds {
        let map_key = Symbol::new(&env, "risk_thresholds");
        let map: Map<Symbol, RiskThresholds> = env
            .storage()
            .instance()
            .get(&map_key)
            .unwrap_or(Map::new(&env));

        map.get(asset_type).unwrap_or(RiskThresholds {
            max_risk_score: 8,
            require_aml_clearance_above: 8,
        })
    }

    // ── Issue #124: compliance data export for regulatory reporting ───────────

    /// Generate a structured compliance report for the given report type and
    /// date range. Read-only — callable by anyone.
    ///
    /// `report_type` should be one of:
    /// - `KYC_VERIFICATIONS`  — entries logged as `kyc_verified`
    /// - `TRANSFER_ACTIVITY`   — entries logged as `compliance_check`
    /// - `BLACKLIST_CHANGES`   — entries logged as `blacklisted` / `unblacklisted`
    /// - `SUSPICIOUS_ACTIVITY` — entries logged as `suspicious`
    ///
    /// The date window (`end - start`) must not exceed 90 days (in seconds).
    pub fn generate_report(env: Env, report_type: Symbol, start: u64, end: u64) -> ComplianceReport {
        // Validate the date range.
        if end <= start {
            panic_with_error!(&env, ComplianceError::InvalidDateRange);
        }
        let max_window: u64 = 90 * 86400;
        if end - start > max_window {
            panic_with_error!(&env, ComplianceError::ReportTooLarge);
        }

        let log: Vec<ReportEntry> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "compliance_log"))
            .unwrap_or(Vec::new(&env));

        let mut entries = Vec::<ReportEntry>::new(&env);

        // Determine which entry types belong to the requested report type.
        let kyc_verified = Symbol::new(&env, "kyc_verified");
        let compliance_check = Symbol::new(&env, "compliance_check");
        let blacklisted = Symbol::new(&env, "blacklisted");
        let unblacklisted = Symbol::new(&env, "unblacklisted");
        let suspicious = Symbol::new(&env, "suspicious");

        let report_kyc = Symbol::new(&env, "KYC_VERIFICATIONS");
        let report_transfer = Symbol::new(&env, "TRANSFER_ACTIVITY");
        let report_blacklist = Symbol::new(&env, "BLACKLIST_CHANGES");
        let report_suspicious = Symbol::new(&env, "SUSPICIOUS_ACTIVITY");

        for entry in log.iter() {
            // Date-range filter (inclusive of [start, end]).
            if entry.timestamp < start || entry.timestamp > end {
                continue;
            }

            let matches = if report_type == report_kyc {
                entry.entry_type == kyc_verified
            } else if report_type == report_transfer {
                entry.entry_type == compliance_check
            } else if report_type == report_blacklist {
                entry.entry_type == blacklisted || entry.entry_type == unblacklisted
            } else if report_type == report_suspicious {
                entry.entry_type == suspicious
            } else {
                // Unknown report type — no matches.
                false
            };

            if matches {
                entries.push_back(entry.clone());
            }
        }

        ComplianceReport {
            report_type,
            start,
            end,
            entries,
        }
    }
}
