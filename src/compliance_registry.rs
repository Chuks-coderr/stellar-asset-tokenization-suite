use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, Address, Env, Map,
    Symbol, Vec,
};

const STORAGE_VERSION: u32 = 1;

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

// ... (around line 58)
#[contract]
pub struct ComplianceRegistry;

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
        env.storage().instance().get(&user).unwrap_or(KYCStatus {
            is_verified: false,
            verification_level: 0,
            expiry_date: 0,
            jurisdiction: Symbol::new(&env, "UNKNOWN"),
            is_accredited: false,
            risk_score: 5,
            aml_flags: Vec::new(&env),
        })
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
            (Symbol::new(&env, "compliance_check"), from, to),
            (amount, result),
        );

        result
    }

    fn check_compliance_internal(env: Env, from: Address, to: Address, amount: i128) -> bool {
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

            // 4. Risk score check — reject high-risk participants (score >= 8 out of 10)
            if from_kyc.risk_score >= 8 || to_kyc.risk_score >= 8 {
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

        let mut limits: TransferLimits = map.get(user.clone()).unwrap_or(TransferLimits {
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
        let day_in_seconds = 86400;
        let month_in_seconds = 30 * day_in_seconds;
        let year_in_seconds = 365 * day_in_seconds;

        if current_time - limits.last_reset_daily >= day_in_seconds {
            limits.remaining_daily = limits.daily_limit;
            limits.last_reset_daily = current_time;
        }

        if current_time - limits.last_reset_monthly >= month_in_seconds {
            limits.remaining_monthly = limits.monthly_limit;
            limits.last_reset_monthly = current_time;
        }

        if current_time - limits.last_reset_annual >= year_in_seconds {
            limits.remaining_annual = limits.annual_limit;
            limits.last_reset_annual = current_time;
        }

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

        let limits: TransferLimits = map.get(user).unwrap_or(TransferLimits {
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
        let day_in_seconds: u64 = 86400;
        let month_in_seconds: u64 = 30 * day_in_seconds;
        let year_in_seconds: u64 = 365 * day_in_seconds;

        // Compute effective remaining after potential resets (without writing)
        let effective_daily = if current_time - limits.last_reset_daily >= day_in_seconds {
            limits.daily_limit
        } else {
            limits.remaining_daily
        };
        let effective_monthly = if current_time - limits.last_reset_monthly >= month_in_seconds {
            limits.monthly_limit
        } else {
            limits.remaining_monthly
        };
        let effective_annual = if current_time - limits.last_reset_annual >= year_in_seconds {
            limits.annual_limit
        } else {
            limits.remaining_annual
        };

        amount <= effective_daily && amount <= effective_monthly && amount <= effective_annual
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
}
