use soroban_sdk::xdr::ToXdr;
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, Address, BytesN, Env, Map,
    String, Symbol, Vec,
};

use crate::auth::assert_admin;
use crate::rwa_token::RWATokenClient;
use crate::RwaDeploySpec;

const STORAGE_VERSION: u32 = 2;

/// Roughly 7 days / 30 days in ledgers (5 second close time).
const INSTANCE_TTL_THRESHOLD: u32 = 17_280 * 7;
const INSTANCE_TTL_EXTEND: u32 = 17_280 * 30;

/// Registry entries are the source of truth for deployed assets, so they live
/// in persistent storage and are bumped on every access.
const PERSISTENT_TTL_THRESHOLD: u32 = 17_280 * 30;
const PERSISTENT_TTL_EXTEND: u32 = 17_280 * 90;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum AssetFactoryError {
    InvalidParameters = 1,
    Unauthorized = 2,
    AssetAlreadyExists = 3,
    ComplianceCheckFailed = 4,
    AssetNotFound = 5,
    UpgradeNotApproved = 6,
    TemplateNotFound = 7,
    GovernanceThresholdNotMet = 8,
    AlreadyInitialized = 9,
    TemplateNotActive = 10,
    NotInitialized = 11,
    Overflow = 12,
    StorageOutdated = 13,
    AlreadyAtLatestVersion = 14,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum AssetClass {
    RealEstate = 0,
    Commodity = 1,
    Invoice = 2,
    Security = 3,
    Art = 4,
    CarbonCredit = 5,
    RenewableEnergy = 6,
}

#[contracttype]
#[derive(Clone)]
pub struct ComplianceRules {
    pub kyc_required: bool,
    pub accredited_investor_only: bool,
    pub geographic_restrictions: Vec<Symbol>,
    pub holding_period_days: u32,
    pub transfer_limits: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DividendSchedule {
    pub frequency_days: u32,
    pub next_distribution_date: u64,
    pub total_distributed: i128,
    pub is_active: bool,
}

#[contracttype]
#[derive(Clone)]
pub struct AssetConfig {
    pub name: Symbol,
    pub symbol: Symbol,
    pub decimals: u32,
    pub total_supply: i128,
    pub asset_class: AssetClass,
    pub compliance_rules: ComplianceRules,
    pub dividend_schedule: soroban_sdk::Vec<DividendSchedule>,
    pub metadata: Map<Symbol, String>,
}

#[contracttype]
#[derive(Clone)]
pub struct AssetTemplate {
    pub asset_class: AssetClass,
    pub base_config: AssetConfig,
    pub wasm_hash: BytesN<32>,
    pub is_active: bool,
    pub version: u32,
}

#[contracttype]
#[derive(Clone)]
pub struct AssetInfo {
    pub name: Symbol,
    pub symbol: Symbol,
    pub total_supply: i128,
    pub decimals: u32,
    pub asset_class: AssetClass,
    pub metadata: Map<Symbol, String>,
    pub compliance_registry: Address,
    pub dividend_distributor: Address,
    pub token_address: Address,
    pub created_at: u64,
    pub is_paused: bool,
    pub template_version: u32,
    pub upgrade_proposals: Vec<Address>,
}

fn registry_key(env: &Env) -> Symbol {
    Symbol::new(env, "registry")
}

fn templates_key(env: &Env) -> Symbol {
    Symbol::new(env, "templates")
}

fn admin_key(env: &Env) -> Symbol {
    Symbol::new(env, "admin")
}

fn asset_count_key(env: &Env) -> Symbol {
    Symbol::new(env, "asset_count")
}

#[contract]
pub struct AssetFactory;

#[contractimpl]
impl AssetFactory {
    pub fn initialize(env: Env, auth: Address, admin: Address) {
        if env.storage().instance().has(&admin_key(&env)) {
            panic_with_error!(&env, AssetFactoryError::AlreadyInitialized);
        }

        // `write_admin` performs the auth check and stores the admin in
        // instance storage (it must run after the duplicate-init check).
        crate::shared_admin::write_admin(&env, &auth, &admin);

        env.storage().instance().set(&asset_count_key(&env), &0u32);
        env.storage()
            .instance()
            .set(&templates_key(&env), &Map::<AssetClass, AssetTemplate>::new(&env));
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "version"), &STORAGE_VERSION);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "governance_threshold"), &6600u32); // 66% in basis points

        Self::write_registry(&env, &Map::<Symbol, AssetInfo>::new(&env));
        Self::extend_instance_ttl(&env);

        env.events().publish(
            (Symbol::new(&env, "factory_initialized"), auth),
            admin,
        );
    }

    // ── storage helpers ───────────────────────────────────────────────────────

    /// Instance storage holds the admin, counters and templates. Bump its TTL
    /// whenever it is touched so it does not expire while the contract is in
    /// active use.
    fn extend_instance_ttl(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND);
    }

    fn read_admin(env: &Env) -> Address {
        Self::extend_instance_ttl(env);
        env.storage()
            .instance()
            .get(&admin_key(env))
            .unwrap_or_else(|| panic_with_error!(env, AssetFactoryError::NotInitialized))
    }

    fn read_registry(env: &Env) -> Map<Symbol, AssetInfo> {
        let key = registry_key(env);
        let registry = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Map::new(env));

        // Every read refreshes the persistent entry's TTL.
        if env.storage().persistent().has(&key) {
            env.storage()
                .persistent()
                .extend_ttl(&key, PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND);
        }

        registry
    }

    fn write_registry(env: &Env, registry: &Map<Symbol, AssetInfo>) {
        let key = registry_key(env);
        env.storage().persistent().set(&key, registry);
        env.storage()
            .persistent()
            .extend_ttl(&key, PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND);
    }

    fn read_templates(env: &Env) -> Map<AssetClass, AssetTemplate> {
        Self::extend_instance_ttl(env);
        env.storage()
            .instance()
            .get(&templates_key(env))
            .unwrap_or_else(|| Map::new(env))
    }

    fn write_templates(env: &Env, templates: &Map<AssetClass, AssetTemplate>) {
        env.storage().instance().set(&templates_key(env), templates);
        Self::extend_instance_ttl(env);
    }

    fn read_asset_count(env: &Env) -> u32 {
        Self::extend_instance_ttl(env);
        env.storage()
            .instance()
            .get(&asset_count_key(env))
            .unwrap_or(0u32)
    }

    fn increment_asset_count(env: &Env) {
        let next = Self::read_asset_count(env)
            .checked_add(1)
            .unwrap_or_else(|| panic_with_error!(env, AssetFactoryError::Overflow));
        env.storage().instance().set(&asset_count_key(env), &next);
        Self::extend_instance_ttl(env);
    }

    fn read_version(env: &Env) -> u32 {
        env.storage()
            .instance()
            .get(&Symbol::new(env, "version"))
            .unwrap_or(0)
    }

    fn check_version(env: &Env) {
        if Self::read_version(env) < STORAGE_VERSION {
            panic_with_error!(env, AssetFactoryError::StorageOutdated);
        }
    }

    pub fn migrate(env: Env, auth: Address) {
        let admin = Self::read_admin(&env);
        assert_admin(&env, &auth, &admin);
        Self::extend_instance_ttl(&env);

        let ver = Self::read_version(&env);
        if ver >= STORAGE_VERSION {
            panic_with_error!(&env, AssetFactoryError::AlreadyAtLatestVersion);
        }

        // v1 -> v2: move the registry from instance storage to persistent
        // storage so deployed-asset data survives instance TTL expiry.
        if ver < 2 {
            let key = registry_key(&env);
            if !env.storage().persistent().has(&key) {
                if let Some(registry) = env
                    .storage()
                    .instance()
                    .get::<Symbol, Map<Symbol, AssetInfo>>(&key)
                {
                    env.storage().persistent().set(&key, &registry);
                    env.storage()
                        .persistent()
                        .extend_ttl(&key, PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND);
                } else {
                    Self::write_registry(&env, &Map::new(&env));
                }
            }
            if env.storage().instance().has(&key) {
                env.storage().instance().remove(&key);
            }
        }

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "version"), &STORAGE_VERSION);
        Self::extend_instance_ttl(&env);
    }

    /// Create a new RWA asset with deterministic address deployment
    pub fn create_asset(env: Env, auth: Address, config: AssetConfig) -> Address {
        let admin = Self::read_admin(&env);
        assert_admin(&env, &auth, &admin);

        Self::check_version(&env);

        // Validate parameters
        if config.total_supply <= 0 || config.decimals > 18 {
            panic_with_error!(&env, AssetFactoryError::InvalidParameters);
        }

        // Validate compliance rules
        if config.compliance_rules.transfer_limits < 0 {
            panic_with_error!(&env, AssetFactoryError::InvalidParameters);
        }

        // Validate dividend schedule if present
        if let Some(schedule) = config.dividend_schedule.get(0) {
            if schedule.frequency_days == 0
                || schedule.next_distribution_date < env.ledger().timestamp()
            {
                panic_with_error!(&env, AssetFactoryError::InvalidParameters);
            }
            if schedule.total_distributed < 0 {
                panic_with_error!(&env, AssetFactoryError::InvalidParameters);
            }
        }

        let symbol = config.symbol.clone();
        let mut registry = Self::read_registry(&env);

        // Check if asset already exists
        if registry.contains_key(symbol.clone()) {
            panic_with_error!(&env, AssetFactoryError::AssetAlreadyExists);
        }

        // Get template for asset class
        let templates = Self::read_templates(&env);

        let template = templates
            .get(config.asset_class.clone())
            .unwrap_or_else(|| panic_with_error!(&env, AssetFactoryError::TemplateNotFound));

        if !template.is_active {
            panic_with_error!(&env, AssetFactoryError::TemplateNotActive);
        }

        // Validate asset class symbol
        let asset_class_str = format!("{:?}", config.asset_class);
        if !is_valid_symbol(&asset_class_str) {
            panic_with_error!(&env, AssetFactoryError::InvalidParameters);
        }
        let asset_type_sym = Symbol::new(&env, &asset_class_str);

        // Deploy token with deterministic address using salt
        let salt = env.crypto().sha256(&symbol.clone().to_xdr(&env));

        let token_address = env
            .deployer()
            .with_current_contract(salt)
            .deploy_v2(
                template.wasm_hash,
                soroban_sdk::Vec::<soroban_sdk::Val>::new(&env),
            );

        // Initialize token
        let token_client = RWATokenClient::new(&env, &token_address);
        token_client.initialize(
            &auth,
            &config.name,
            &symbol,
            &config.total_supply,
            &config.decimals,
            &asset_type_sym,
            &config.metadata,
            &env.current_contract_address(), // Placeholder compliance registry
            &env.current_contract_address(), // Placeholder dividend distributor
        );

        // Create asset info
        let asset_info = AssetInfo {
            name: config.name,
            symbol: symbol.clone(),
            total_supply: config.total_supply,
            decimals: config.decimals,
            asset_class: config.asset_class,
            metadata: config.metadata,
            compliance_registry: env.current_contract_address(),
            dividend_distributor: env.current_contract_address(),
            token_address: token_address.clone(),
            created_at: env.ledger().timestamp(),
            is_paused: false,
            template_version: template.version,
            upgrade_proposals: Vec::new(&env),
        };

        // Update registry — single source of truth for all asset data
        registry.set(symbol, asset_info);
        Self::write_registry(&env, &registry);
        Self::increment_asset_count(&env);

        token_address
    }

    /// Link and initialize a `token_contract` that was already deployed on-chain.
    pub fn deploy_rwa_token(env: Env, auth: Address, spec: RwaDeploySpec) -> Address {
        if spec.total_supply <= 0 || spec.decimals > 18 {
            panic_with_error!(&env, AssetFactoryError::InvalidParameters);
        }

        let admin = Self::read_admin(&env);
        assert_admin(&env, &auth, &admin);

        Self::check_version(&env);

        let symbol = spec.asset_symbol.clone();
        let mut registry = Self::read_registry(&env);

        if registry.contains_key(symbol.clone()) {
            panic_with_error!(&env, AssetFactoryError::AssetAlreadyExists);
        }

        let token_client = RWATokenClient::new(&env, &spec.token_contract);
        token_client.initialize(
            &auth,
            &spec.asset_name,
            &symbol,
            &spec.total_supply,
            &spec.decimals,
            &spec.asset_type,
            &spec.metadata,
            &spec.compliance_registry,
            &spec.dividend_distributor,
        );

        let asset_info = AssetInfo {
            name: spec.asset_name,
            symbol: symbol.clone(),
            total_supply: spec.total_supply,
            decimals: spec.decimals,
            asset_class: AssetClass::Security, // default; caller should use create_asset for typed classes
            metadata: spec.metadata,
            compliance_registry: spec.compliance_registry,
            dividend_distributor: spec.dividend_distributor,
            token_address: spec.token_contract.clone(),
            created_at: env.ledger().timestamp(),
            is_paused: false,
            template_version: 0,
            upgrade_proposals: Vec::new(&env),
        };

        // Write into the registry Map — single source of truth
        registry.set(symbol, asset_info);
        Self::write_registry(&env, &registry);
        Self::increment_asset_count(&env);

        spec.token_contract
    }

    pub fn get_asset_info(env: Env, symbol: Symbol) -> AssetInfo {
        let registry = Self::read_registry(&env);

        registry
            .get(symbol)
            .unwrap_or_else(|| panic_with_error!(&env, AssetFactoryError::AssetNotFound))
    }

    /// List all assets in the registry
    pub fn list_assets(env: Env) -> Vec<AssetInfo> {
        let registry = Self::read_registry(&env);

        let mut result = Vec::<AssetInfo>::new(&env);
        for (_symbol, asset_info) in registry.iter() {
            result.push_back(asset_info);
        }
        result
    }

    pub fn set_asset_pause_status(env: Env, auth: Address, symbol: Symbol, paused: bool) {
        let admin = Self::read_admin(&env);
        assert_admin(&env, &auth, &admin);

        Self::check_version(&env);

        let mut registry = Self::read_registry(&env);

        let mut asset_info = registry
            .get(symbol.clone())
            .unwrap_or_else(|| panic_with_error!(&env, AssetFactoryError::AssetNotFound));

        asset_info.is_paused = paused;
        registry.set(symbol.clone(), asset_info);
        Self::write_registry(&env, &registry);

        env.events().publish(
            (Symbol::new(&env, "asset_pause_status_changed"), symbol, auth),
            paused,
        );
    }

    pub fn update_admin(env: Env, auth: Address, new_admin: Address) {
        let admin = Self::read_admin(&env);
        assert_admin(&env, &auth, &admin);

        Self::check_version(&env);

        env.storage().instance().set(&admin_key(&env), &new_admin);
        Self::extend_instance_ttl(&env);

        env.events().publish(
            (Symbol::new(&env, "factory_admin_updated"), auth),
            new_admin,
        );
    }

    /// Register a new template for an asset class
    pub fn register_template(env: Env, auth: Address, template: AssetTemplate) {
        let admin = Self::read_admin(&env);
        assert_admin(&env, &auth, &admin);

        Self::check_version(&env);

        let mut templates = Self::read_templates(&env);
        templates.set(template.asset_class.clone(), template.clone());
        Self::write_templates(&env, &templates);

        env.events().publish(
            (Symbol::new(&env, "template_registered"), auth),
            (template.asset_class, template.version, template.is_active),
        );
    }

    /// Get template for a specific asset class
    pub fn get_template(env: Env, asset_class: AssetClass) -> AssetTemplate {
        let templates = Self::read_templates(&env);

        templates
            .get(asset_class)
            .unwrap_or_else(|| panic_with_error!(&env, AssetFactoryError::TemplateNotFound))
    }

    /// Upgrade an asset with governance approval
    pub fn upgrade_asset(env: Env, auth: Address, symbol: Symbol, new_wasm_hash: BytesN<32>) {
        let admin = Self::read_admin(&env);
        assert_admin(&env, &auth, &admin);

        Self::check_version(&env);

        let mut registry = Self::read_registry(&env);

        let mut asset_info = registry
            .get(symbol.clone())
            .unwrap_or_else(|| panic_with_error!(&env, AssetFactoryError::AssetNotFound));

        // Check governance threshold (simplified - in real implementation would check token holder votes)
        let _threshold: u32 = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "governance_threshold"))
            .unwrap_or(6600u32);

        // For now, assume admin approval meets threshold
        // In production, this would involve actual voting mechanism

        // Deploy new contract version
        let salt = env.crypto().sha256(&symbol.clone().to_xdr(&env));

        let new_token_address = env
            .deployer()
            .with_current_contract(salt)
            .deploy_v2(
                new_wasm_hash,
                soroban_sdk::Vec::<soroban_sdk::Val>::new(&env),
            );

        // Update asset info
        asset_info.token_address = new_token_address.clone();
        asset_info.template_version = asset_info
            .template_version
            .checked_add(1)
            .unwrap_or_else(|| panic_with_error!(&env, AssetFactoryError::Overflow));
        let new_template_version = asset_info.template_version;

        // Update registry
        registry.set(symbol.clone(), asset_info);
        Self::write_registry(&env, &registry);

        env.events().publish(
            (Symbol::new(&env, "asset_upgraded"), symbol, auth),
            (new_token_address, new_template_version),
        );
    }

    /// Emergency pause all assets
    pub fn emergency_pause_all(env: Env, auth: Address) {
        let admin = Self::read_admin(&env);
        assert_admin(&env, &auth, &admin);

        Self::check_version(&env);

        let registry = Self::read_registry(&env);

        let mut updated_registry = Map::<Symbol, AssetInfo>::new(&env);

        for (symbol, mut asset_info) in registry.iter() {
            asset_info.is_paused = true;
            updated_registry.set(symbol, asset_info);
        }

        Self::write_registry(&env, &updated_registry);

        let count: u32 = registry.len();
        env.events().publish(
            (Symbol::new(&env, "emergency_pause_all"), auth),
            count,
        );
    }

    /// Get all assets from registry
    pub fn get_all_assets(env: Env) -> Map<Symbol, AssetInfo> {
        Self::read_registry(&env)
    }

    /// Get asset count — kept in instance storage as a cheap counter.
    pub fn get_asset_count(env: Env) -> u32 {
        Self::read_asset_count(&env)
    }
}

pub fn is_valid_symbol(s: &str) -> bool {
    if s.is_empty() || s.len() > 32 {
        return false;
    }
    for c in s.chars() {
        if !c.is_ascii_alphanumeric() && c != '_' {
            return false;
        }
    }
    true
}
