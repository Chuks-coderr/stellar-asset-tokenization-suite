use soroban_sdk::xdr::ToXdr;
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, Address, Env, Map, Symbol, Vec, BytesN, String,
};

use crate::rwa_token::RWATokenClient;
use crate::RwaDeploySpec;
use crate::auth::assert_admin;

const STORAGE_VERSION: u32 = 1;

/// Maximum number of metadata entries an asset may carry (Issue #82).
const MAX_METADATA_ENTRIES: u32 = 50;

/// Maximum length, in bytes, of a metadata value (Issue #82).
const MAX_METADATA_VALUE_LEN: u32 = 256;

/// Default number of assets returned by `query_assets` (Issue #83).
const DEFAULT_QUERY_PAGE_SIZE: u32 = 20;

/// Maximum number of assets `query_assets` returns in one page (Issue #83).
const MAX_QUERY_PAGE_SIZE: u32 = 100;

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

/// The filters applied by `query_assets` (Issue #83).
///
/// A filter that is not switched on is not applied, so one struct covers
/// "no filter" and "filter by class" without optional parameters.
#[contracttype]
#[derive(Clone)]
pub struct AssetQueryFilter {
    /// Only return assets of this class, when `by_class` is set.
    pub asset_class: AssetClass,
    /// Whether the asset class filter applies.
    pub by_class: bool,
    /// Only return assets in this pause state, when `by_pause_state` is set.
    pub is_paused: bool,
    /// Whether the pause state filter applies.
    pub by_pause_state: bool,
    /// Only return assets created at or after this timestamp, when
    /// `by_created_at` is set.
    pub created_after: u64,
    /// Only return assets created at or before this timestamp, when
    /// `by_created_at` is set.
    pub created_before: u64,
    /// Whether the created-at window applies.
    pub by_created_at: bool,
    /// Resume the query after the asset with this symbol, when `has_cursor`
    /// is set. The registry is keyed by symbol, so the order is stable.
    pub cursor: Symbol,
    /// Whether the cursor applies.
    pub has_cursor: bool,
}

/// A page of assets returned by `query_assets` (Issue #83).
#[contracttype]
#[derive(Clone)]
pub struct AssetQueryPage {
    /// The assets on this page.
    pub assets: Vec<AssetInfo>,
    /// The symbol to pass back as the cursor to fetch the next page: the
    /// symbol of the last asset of this page.
    pub next_cursor: Symbol,
    /// Whether another page follows this one.
    pub has_more: bool,
}

#[contract]
pub struct AssetFactory;

#[contractimpl]
impl AssetFactory {
    pub fn initialize(env: Env, auth: Address, admin: Address) {
        crate::shared_admin::write_admin(&env, &auth, &admin);
        
        auth.require_auth();
        if env.storage().instance().has(&Symbol::new(&env, "admin")) {
            panic_with_error!(&env, AssetFactoryError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "admin"), &admin);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "asset_count"), &0u32);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "templates"), &Map::<AssetClass, AssetTemplate>::new(&env));
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "registry"), &Map::<Symbol, AssetInfo>::new(&env));
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "version"), &STORAGE_VERSION);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "governance_threshold"), &6600u32); // 66% in basis points

        env.events().publish(
            (Symbol::new(&env, "factory_initialized"), auth),
            admin,
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
            panic_with_error!(env, AssetFactoryError::StorageOutdated);
        }
    }

    pub fn migrate(env: Env, auth: Address) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "admin"))
            .unwrap_or_else(|| { panic_with_error!(&env, AssetFactoryError::NotInitialized); });

        assert_admin(&env, &auth, &admin);

        let ver = Self::read_version(&env);
        if ver >= STORAGE_VERSION {
            panic_with_error!(&env, AssetFactoryError::AlreadyAtLatestVersion);
        }

        let mut current = ver;
        while current < STORAGE_VERSION {
            current += 1;
        }

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "version"), &STORAGE_VERSION);
    }

    /// Create a new RWA asset with deterministic address deployment
    pub fn create_asset(env: Env, auth: Address, config: AssetConfig) -> Address {
        let admin: Address = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "admin"))
            .unwrap_or_else(|| { panic_with_error!(&env, AssetFactoryError::NotInitialized); });

        assert_admin(&env, &auth, &admin);

        Self::check_version(&env);

        // Validate parameters
        if config.total_supply <= 0 || config.decimals > 18 {
            panic_with_error!(&env, AssetFactoryError::InvalidParameters);
        }

        // Issue #82 — bound the metadata so a single asset cannot bloat the
        // contract's storage.
        validate_metadata(&env, &config.metadata);

        // Validate compliance rules
        if config.compliance_rules.transfer_limits < 0 {
            panic_with_error!(&env, AssetFactoryError::InvalidParameters);
        }

        // Validate dividend schedule if present
        if let Some(schedule) = config.dividend_schedule.get(0) {
            if schedule.frequency_days == 0 || schedule.next_distribution_date < env.ledger().timestamp() {
                panic_with_error!(&env, AssetFactoryError::InvalidParameters);
            }
            if schedule.total_distributed < 0 {
                panic_with_error!(&env, AssetFactoryError::InvalidParameters);
            }
        }

        // Check if asset already exists
        let registry: Map<Symbol, AssetInfo> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "registry"))
            .unwrap_or(Map::new(&env));
        
        if registry.contains_key(config.symbol.clone()) {
            panic_with_error!(&env, AssetFactoryError::AssetAlreadyExists);
        }

        // Get template for asset class
        let templates: Map<AssetClass, AssetTemplate> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "templates"))
            .unwrap_or(Map::new(&env));
        
        let template = templates.get(config.asset_class.clone())
            .unwrap_or_else(|| { panic_with_error!(&env, AssetFactoryError::TemplateNotFound); });
        
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
        let salt = env.crypto().sha256(&config.symbol.to_xdr(&env));
        
        let token_address = env.deployer()
            .with_current_contract(salt)
            .deploy_v2(template.wasm_hash, soroban_sdk::Vec::<soroban_sdk::Val>::new(&env));

        // Initialize token
        let token_client = RWATokenClient::new(&env, &token_address);
        token_client.initialize(
            &auth,
            &config.name,
            &config.symbol,
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
            symbol: config.symbol,
            total_supply: config.total_supply,
            decimals: config.decimals,
            asset_class: config.asset_class,
            metadata: config.metadata.clone(),
            compliance_registry: env.current_contract_address(),
            dividend_distributor: env.current_contract_address(),
            token_address: token_address.clone(),
            created_at: env.ledger().timestamp(),
            is_paused: false,
            template_version: template.version,
            upgrade_proposals: Vec::new(&env),
        };

        // Update registry — single source of truth for all asset data
        let mut registry = registry;
        registry.set(config.symbol, asset_info);
        env.storage().instance().set(&Symbol::new(&env, "registry"), &registry);

        token_address
    }

    /// Link and initialize a `token_contract` that was already deployed on-chain.
    pub fn deploy_rwa_token(env: Env, auth: Address, spec: RwaDeploySpec) -> Address {
        if spec.total_supply <= 0 || spec.decimals > 18 {
            panic_with_error!(&env, AssetFactoryError::InvalidParameters);
        }

        // Issue #82 — bound the metadata so a single asset cannot bloat the
        // contract's storage.
        validate_metadata(&env, &spec.metadata);

        let admin: Address = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "admin"))
            .unwrap_or_else(|| { panic_with_error!(&env, AssetFactoryError::NotInitialized); });

        assert_admin(&env, &auth, &admin);

        Self::check_version(&env);

        let token_client = RWATokenClient::new(&env, &spec.token_contract);
        token_client.initialize(
            &auth,
            &spec.asset_name,
            &spec.asset_symbol,
            &spec.total_supply,
            &spec.decimals,
            &spec.asset_type,
            &spec.metadata,
            &spec.compliance_registry,
            &spec.dividend_distributor,
        );

        let asset_info = AssetInfo {
            name: spec.asset_name,
            symbol: spec.asset_symbol.clone(),
            total_supply: spec.total_supply,
            decimals: spec.decimals,
            asset_class: AssetClass::Security, // default; caller should use create_asset for typed classes
            metadata: spec.metadata.clone(),
            compliance_registry: spec.compliance_registry,
            dividend_distributor: spec.dividend_distributor,
            token_address: spec.token_contract.clone(),
            created_at: env.ledger().timestamp(),
            is_paused: false,
            template_version: 0,
            upgrade_proposals: Vec::new(&env),
        };

        // Write into the registry Map — single source of truth
        let mut registry: Map<Symbol, AssetInfo> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "registry"))
            .unwrap_or(Map::new(&env));

        if registry.contains_key(spec.asset_symbol.clone()) {
            panic_with_error!(&env, AssetFactoryError::AssetAlreadyExists);
        }

        registry.set(spec.asset_symbol, asset_info);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "registry"), &registry);

        spec.token_contract
    }

    pub fn get_asset_info(env: Env, symbol: Symbol) -> AssetInfo {
        let registry: Map<Symbol, AssetInfo> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "registry"))
            .unwrap_or(Map::new(&env));

        registry
            .get(symbol)
            .unwrap_or_else(|| { panic_with_error!(&env, AssetFactoryError::AssetNotFound); })
    }

    /// List all assets in the registry
    pub fn list_assets(env: Env) -> Vec<AssetInfo> {
        let registry: Map<Symbol, AssetInfo> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "registry"))
            .unwrap_or(Map::new(&env));

        let mut result = Vec::<AssetInfo>::new(&env);
        for (_symbol, asset_info) in registry.iter() {
            result.push_back(asset_info);
        }
        result
    }

    /// Query the registry with filters and cursor-based pagination
    /// (Issue #83).
    ///
    /// The registry is keyed by symbol, so the iteration order is stable.
    /// `filter.cursor` is the symbol of the last asset of the previous page
    /// and the query resumes after it; `filter.has_cursor` says whether the
    /// cursor applies at all (the first page is fetched with it off).
    ///
    /// `limit` is the page size: `0` means the default of 20, and anything
    /// above 100 is capped at 100.
    pub fn query_assets(env: Env, filter: AssetQueryFilter, limit: u32) -> AssetQueryPage {
        let page_size = if limit == 0 {
            DEFAULT_QUERY_PAGE_SIZE
        } else if limit > MAX_QUERY_PAGE_SIZE {
            MAX_QUERY_PAGE_SIZE
        } else {
            limit
        };

        let registry: Map<Symbol, AssetInfo> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "registry"))
            .unwrap_or(Map::new(&env));

        let mut assets: Vec<AssetInfo> = Vec::new(&env);
        let mut next_cursor: Symbol = filter.cursor.clone();
        let mut has_more = false;
        let mut skipping = filter.has_cursor;

        for (symbol, asset_info) in registry.iter() {
            if skipping {
                // Resume after the cursor. Comparing the symbols rather than
                // looking the cursor up keeps the page correct even when the
                // asset the cursor points at is gone.
                if symbol > filter.cursor {
                    skipping = false;
                } else {
                    continue;
                }
            }

            if !Self::matches_filters(&asset_info, &filter) {
                continue;
            }

            if assets.len() >= page_size {
                // The page is full and another asset matches, so there is a
                // next page and the cursor is the last symbol of this one.
                has_more = true;
                break;
            }

            assets.push_back(asset_info);
            next_cursor = symbol.clone();
        }

        AssetQueryPage { assets, next_cursor, has_more }
    }

    /// Whether `asset` passes the filters of `filter` (Issue #83).
    fn matches_filters(asset: &AssetInfo, filter: &AssetQueryFilter) -> bool {
        if filter.by_class && asset.asset_class != filter.asset_class.clone() {
            return false;
        }

        if filter.by_pause_state && asset.is_paused != filter.is_paused {
            return false;
        }

        if filter.by_created_at {
            if asset.created_at < filter.created_after {
                return false;
            }
            if asset.created_at > filter.created_before {
                return false;
            }
        }

        true
    }

    pub fn set_asset_pause_status(env: Env, auth: Address, symbol: Symbol, paused: bool) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "admin"))
            .unwrap_or_else(|| { panic_with_error!(&env, AssetFactoryError::NotInitialized); });

        assert_admin(&env, &auth, &admin);

        Self::check_version(&env);

        let mut registry: Map<Symbol, AssetInfo> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "registry"))
            .unwrap_or(Map::new(&env));

        let mut asset_info = registry
            .get(symbol.clone())
            .unwrap_or_else(|| { panic_with_error!(&env, AssetFactoryError::AssetNotFound); });

        asset_info.is_paused = paused;
        registry.set(symbol.clone(), asset_info);
        env.storage().instance().set(&Symbol::new(&env, "registry"), &registry);

        env.events().publish(
            (Symbol::new(&env, "asset_pause_status_changed"), symbol, auth),
            paused,
        );
    }

    pub fn update_admin(env: Env, auth: Address, new_admin: Address) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "admin"))
            .unwrap_or_else(|| { panic_with_error!(&env, AssetFactoryError::NotInitialized); });

        assert_admin(&env, &auth, &admin);

        Self::check_version(&env);

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "admin"), &new_admin);

        env.events().publish(
            (Symbol::new(&env, "factory_admin_updated"), auth),
            new_admin,
        );
    }

    /// Register a new template for an asset class
    pub fn register_template(env: Env, auth: Address, template: AssetTemplate) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "admin"))
            .unwrap_or_else(|| { panic_with_error!(&env, AssetFactoryError::NotInitialized); });

        assert_admin(&env, &auth, &admin);

        Self::check_version(&env);

        let mut templates: Map<AssetClass, AssetTemplate> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "templates"))
            .unwrap_or(Map::new(&env));
        
        templates.set(template.asset_class.clone(), template.clone());
        env.storage().instance().set(&Symbol::new(&env, "templates"), &templates);

        env.events().publish(
            (Symbol::new(&env, "template_registered"), auth),
            (template.asset_class, template.version, template.is_active),
        );
    }

    /// Get template for a specific asset class
    pub fn get_template(env: Env, asset_class: AssetClass) -> AssetTemplate {
        let templates: Map<AssetClass, AssetTemplate> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "templates"))
            .unwrap_or(Map::new(&env));
        
        templates.get(asset_class)
            .unwrap_or_else(|| { panic_with_error!(&env, AssetFactoryError::TemplateNotFound); })
    }

    /// Upgrade an asset with governance approval
    pub fn upgrade_asset(env: Env, auth: Address, symbol: Symbol, new_wasm_hash: BytesN<32>) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "admin"))
            .unwrap_or_else(|| { panic_with_error!(&env, AssetFactoryError::NotInitialized); });

        assert_admin(&env, &auth, &admin);

        Self::check_version(&env);

        let registry: Map<Symbol, AssetInfo> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "registry"))
            .unwrap_or(Map::new(&env));
        
        let mut asset_info = registry.get(symbol.clone())
            .unwrap_or_else(|| { panic_with_error!(&env, AssetFactoryError::AssetNotFound); });

        // Check governance threshold (simplified - in real implementation would check token holder votes)
        let threshold: u32 = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "governance_threshold"))
            .unwrap_or(6600u32);

        // For now, assume admin approval meets threshold
        // In production, this would involve actual voting mechanism

        // Deploy new contract version
        let salt = env.crypto().sha256(&symbol.to_xdr(&env));
        
        let new_token_address = env.deployer()
            .with_current_contract(salt)
            .deploy_v2(new_wasm_hash, soroban_sdk::Vec::<soroban_sdk::Val>::new(&env));

        // Update asset info
        asset_info.token_address = new_token_address;
        asset_info.template_version = asset_info.template_version
            .checked_add(1)
            .unwrap_or_else(|| panic_with_error!(&env, AssetFactoryError::Overflow));

        // Update registry
        let mut registry = registry;
        registry.set(symbol.clone(), asset_info);
        env.storage().instance().set(&Symbol::new(&env, "registry"), &registry);

        env.events().publish(
            (Symbol::new(&env, "asset_upgraded"), symbol, auth),
            (new_token_address, asset_info.template_version),
        );
    }

    /// Emergency pause all assets
    pub fn emergency_pause_all(env: Env, auth: Address) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "admin"))
            .unwrap_or_else(|| { panic_with_error!(&env, AssetFactoryError::NotInitialized); });

        assert_admin(&env, &auth, &admin);

        Self::check_version(&env);

        let registry: Map<Symbol, AssetInfo> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "registry"))
            .unwrap_or(Map::new(&env));

        let mut updated_registry = Map::<Symbol, AssetInfo>::new(&env);
        
        for (symbol, mut asset_info) in registry.iter() {
            asset_info.is_paused = true;
            updated_registry.set(symbol, asset_info);
        }

        env.storage().instance().set(&Symbol::new(&env, "registry"), &updated_registry);

        let count: u32 = registry.len();
        env.events().publish(
            (Symbol::new(&env, "emergency_pause_all"), auth),
            count,
        );
    }

    /// Get all assets from registry
    pub fn get_all_assets(env: Env) -> Map<Symbol, AssetInfo> {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "registry"))
            .unwrap_or(Map::new(&env))
    }

    /// Get asset count
    pub fn get_asset_count(env: Env) -> u32 {
        let registry: Map<Symbol, AssetInfo> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "registry"))
            .unwrap_or(Map::new(&env));
        registry.len()
    }
}

/// Validates the metadata map of an asset (Issue #82).
///
/// The map is bounded so that a single asset cannot bloat the contract's
/// storage: at most `MAX_METADATA_ENTRIES` entries, each holding a value of
/// at most `MAX_METADATA_VALUE_LEN` bytes.
///
/// The key length is bounded to 32 characters as well, but that bound is
/// enforced by the host - a Soroban `Symbol` can never hold more than 32
/// characters - so an over-long key cannot reach the contract.
pub fn validate_metadata(env: &Env, metadata: &Map<Symbol, String>) {
    if metadata.len() > MAX_METADATA_ENTRIES {
        panic_with_error!(env, AssetFactoryError::InvalidParameters);
    }

    for value in metadata.values().iter() {
        if value.len() > MAX_METADATA_VALUE_LEN {
            panic_with_error!(env, AssetFactoryError::InvalidParameters);
        }
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
