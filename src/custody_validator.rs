use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, Address, Bytes, BytesN,
    Env, Map, Symbol, Vec,
};

use crate::auth::assert_admin;

const STORAGE_VERSION: u32 = 1;

/// Issue #168: hard cap on the number of entries processed by any single
/// iteration, keeping the worst-case gas cost of a call bounded no matter how
/// large the underlying collections grow.
pub const MAX_ITERATIONS: u32 = 100;

/// Expected verification latency (7 days) used when scoring custodian
/// timeliness. Averages at or below this threshold score full marks.
const EXPECTED_VERIFICATION_TIME: u64 = 7 * 86400;

#[contracttype]
pub enum StorageKey {
    Custodian(Address),
    Attestation(u64),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum CustodyError {
    Unauthorized = 1,
    InvalidProof = 2,
    OracleOffline = 3,
    VerificationFailed = 4,
    AssetNotRegistered = 5,
    StaleData = 6,
    InvalidSignature = 7,
    DisputeAlreadyExists = 8,
    InsufficientBond = 9,
    DisputeNotFound = 10,
    InvalidDisputeStatus = 11,
    BondNotRefundable = 12,
    CustodianNotWhitelisted = 13,
    InvalidVerificationType = 14,
    ProofHashMismatch = 15,
    AttestationExpired = 16,
    MultiSigThresholdNotMet = 17,
    InvalidMerkleProof = 18,
    ZKVerificationFailed = 19,
    InsuranceClaimFailed = 20,
    AlreadyInitialized = 21,
    NotInitialized = 22,
    AttestationNotFound = 23,
    OracleNotFound = 24,
    ConfigNotFound = 25,
    StorageOutdated = 26,
    ValidatorNotInitialized = 27,
    AlreadyAtLatestVersion = 28,
    InvalidAttestation = 29,
    CustodianNotFound = 30,
    InvalidParameters = 31,
    MultiSigConfigNotFound = 32,
    DuplicateSigner = 33,
    SensorNotFound = 34,
    SensorInactive = 35,
    InvalidSensorData = 36,
    ThresholdNotFound = 37,
    BridgeNotFound = 38,
    BridgeAlreadyRegistered = 39,
    CrossChainProofNotFound = 40,
    InvalidCrossChainProof = 41,
}

#[contracttype]
#[derive(Clone)]
pub struct CustodyAttestation {
    pub asset_id: Address,
    pub custodian: Address,
    pub location: Symbol,
    pub condition: Symbol,
    pub value: i128,
    pub timestamp: u64,
    pub proof_hash: BytesN<32>,
    pub verification_type: Symbol,
    pub insurance_status: Symbol,
    pub legal_title_hash: BytesN<32>,
    pub audit_report_hash: BytesN<32>,
    pub multi_sig_signatures: Vec<BytesN<64>>,
    pub metadata: Map<Symbol, Symbol>,
    pub is_valid: bool,
    pub expires_at: u64,
}

#[contracttype]
#[derive(Clone)]
pub struct CustodianRegistry {
    pub custodian_address: Address,
    pub name: Symbol,
    pub jurisdiction: Symbol,
    pub license_number: Symbol,
    pub reputation_score: u32,
    pub verification_types: Vec<Symbol>,
    pub is_active: bool,
    pub total_attestations: u64,
    pub successful_disputes: u64,
    pub failed_disputes: u64,
    pub bond_required: i128,
    pub insurance_provider: Symbol,
}

#[contracttype]
#[derive(Clone)]
pub struct CustodyProof {
    pub proof_id: u64,
    pub asset_address: Address,
    pub asset_type: Symbol,
    pub custody_provider: Address,
    pub verification_timestamp: u64,
    pub expiry_timestamp: u64,
    pub asset_value: i128,
    pub asset_location: Symbol,
    pub legal_title: Symbol,
    pub insurance_coverage: i128,
    pub audit_report_hash: BytesN<32>,
    pub oracle_signatures: Vec<BytesN<64>>,
    pub metadata: Map<Symbol, Symbol>,
    pub is_valid: bool,
}

#[contracttype]
#[derive(Clone)]
pub struct OracleInfo {
    pub oracle_address: Address,
    pub name: Symbol,
    pub jurisdiction: Symbol,
    pub verification_methods: Vec<Symbol>,
    pub reputation_score: u32,
    pub fee_rate: i64,
    pub is_active: bool,
    pub last_verification: u64,
    pub total_verifications: u64,
    pub multi_sig_threshold: u32,
    pub auditor_type: Symbol,
    pub license_valid_until: u64,
}

#[contracttype]
#[derive(Clone)]
pub struct DisputeRecord {
    pub dispute_id: u64,
    pub attestation_id: u64,
    pub challenger: Address,
    pub custodian: Address,
    pub reason: Symbol,
    pub bond_amount: i128,
    pub evidence_hash: BytesN<32>,
    pub status: Symbol,
    pub created_at: u64,
    pub resolved_at: u64,
    pub resolution: Symbol,
    pub bond_returned: bool,
    pub penalty_applied: bool,
    pub penalty_amount: i128,
}

#[contracttype]
#[derive(Clone)]
pub struct VerificationTypeConfig {
    pub verification_type: Symbol,
    pub required_documents: Vec<Symbol>,
    pub verification_frequency: u64,
    pub multi_sig_required: bool,
    pub sig_threshold: u32,
    pub insurance_required: bool,
    pub min_insurance_coverage: i128,
    pub iot_monitoring_required: bool,
    pub satellite_verification: bool,
    pub legal_verification_required: bool,
    pub audit_required: bool,
}

#[contracttype]
#[derive(Clone)]
pub struct InsuranceIntegration {
    pub provider: Symbol,
    pub policy_number: Symbol,
    pub coverage_amount: i128,
    pub premium_amount: i128,
    pub valid_until: u64,
    pub claim_auto_trigger: bool,
    pub last_premium_paid: u64,
    pub is_active: bool,
}

#[contracttype]
#[derive(Clone)]
pub struct AssetRegistration {
    pub asset_address: Address,
    pub asset_type: Symbol,
    pub legal_identifier: Symbol,
    pub jurisdiction: Symbol,
    pub custody_requirements: Vec<Symbol>,
    pub verification_frequency: u64,
    pub required_oracles: u32,
    pub last_verified: u64,
    pub is_active: bool,
}

#[contracttype]
#[derive(Clone)]
pub struct ValidationConfig {
    pub min_oracle_reputation: u32,
    pub max_proof_age: u64,
    pub required_verification_methods: Vec<Symbol>,
    pub insurance_required: bool,
    pub min_insurance_coverage: i128,
    pub audit_required: bool,
    pub multi_oracle_required: bool,
    pub oracle_consensus_threshold: u32,
}

/// Issue #166: immutable audit-trail entry. One is appended for every custody
/// operation so compliance officers can reconstruct the full history of an
/// asset's custody without relying on off-chain logs.
#[contracttype]
#[derive(Clone)]
pub struct CustodyAuditEvent {
    pub event_type: Symbol,
    pub timestamp: u64,
    pub actor: Address,
    pub asset_id: Address,
    pub details_hash: BytesN<32>,
}

/// Issue #167: weighted performance score for a custodian.
///
/// * `accuracy`     - share of attestations that were not successfully disputed.
/// * `timeliness`   - average verification latency relative to the expected window.
/// * `thoroughness` - verification methods evidenced vs. methods required.
/// * `total_score`  - weighted average of the three component scores.
#[contracttype]
#[derive(Clone)]
pub struct PerformanceScore {
    pub accuracy: u32,
    pub timeliness: u32,
    pub thoroughness: u32,
    pub accuracy_weight: u32,
    pub timeliness_weight: u32,
    pub thoroughness_weight: u32,
    pub total_score: u32,
}

/// Issue #167: configurable weights used when combining performance metrics.
#[contracttype]
#[derive(Clone)]
pub struct PerformanceWeights {
    pub accuracy: u32,
    pub timeliness: u32,
    pub thoroughness: u32,
}

/// Issue #167: raw counters accumulated per custodian. Kept separate from
/// `CustodianRegistry` so scoring can evolve without breaking the registry
/// layout relied upon by existing migrations.
#[contracttype]
#[derive(Clone)]
pub struct CustodianMetrics {
    pub attestations_scored: u64,
    pub total_latency: u64,
    pub methods_evidenced: u64,
    pub methods_possible: u64,
}

#[contract]
pub struct CustodyValidator;

#[contractimpl]
impl CustodyValidator {
    fn extend_custodian_ttl(env: &Env, address: &Address) {
        env.storage().persistent().extend_ttl(
            &StorageKey::Custodian(address.clone()),
            50000,
            535680,
        );
    }

    fn read_custodian(env: &Env, address: &Address) -> Option<CustodianRegistry> {
        let result = env
            .storage()
            .persistent()
            .get::<StorageKey, CustodianRegistry>(&StorageKey::Custodian(address.clone()));
        if result.is_some() {
            Self::extend_custodian_ttl(env, address);
        }
        result
    }

    fn write_custodian(env: &Env, address: &Address, custodian: &CustodianRegistry) {
        env.storage()
            .persistent()
            .set(&StorageKey::Custodian(address.clone()), custodian);
        Self::extend_custodian_ttl(env, address);
    }

    fn extend_attestation_ttl(env: &Env, id: &u64) {
        env.storage()
            .persistent()
            .extend_ttl(&StorageKey::Attestation(*id), 50000, 535680);
    }

    fn read_attestation(env: &Env, id: &u64) -> Option<CustodyAttestation> {
        let result = env
            .storage()
            .persistent()
            .get::<StorageKey, CustodyAttestation>(&StorageKey::Attestation(*id));
        if result.is_some() {
            Self::extend_attestation_ttl(env, id);
        }
        result
    }

    fn write_attestation(env: &Env, id: &u64, attestation: &CustodyAttestation) {
        env.storage()
            .persistent()
            .set(&StorageKey::Attestation(*id), attestation);
        Self::extend_attestation_ttl(env, id);
    }

    fn put_oracle(env: Env, oracle_address: Address, name: Symbol, jurisdiction: Symbol) {
        let mut methods = Vec::<Symbol>::new(&env);
        methods.push_back(Symbol::new(&env, "physical_inspection"));
        methods.push_back(Symbol::new(&env, "document_verification"));
        methods.push_back(Symbol::new(&env, "blockchain_audit"));

        let oracle_info = OracleInfo {
            oracle_address: oracle_address.clone(),
            name,
            jurisdiction,
            verification_methods: methods,
            reputation_score: 80,
            fee_rate: 25,
            is_active: true,
            last_verification: env.ledger().timestamp(),
            total_verifications: 0,
            multi_sig_threshold: 2,
            auditor_type: Symbol::new(&env, "external"),
            license_valid_until: env.ledger().timestamp() + 86400 * 365,
        };

        let mut oracles: Map<Address, OracleInfo> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "oracles"))
            .unwrap_or(Map::new(&env));

        oracles.set(oracle_address, oracle_info);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "oracles"), &oracles);
    }

    pub fn initialize(env: Env, auth: Address, admin: Address, oracle_addresses: Vec<Address>) {
        auth.require_auth();
        if env
            .storage()
            .instance()
            .has(&Symbol::new(&env, "initialized"))
        {
            panic_with_error!(&env, CustodyError::AlreadyInitialized);
        }

        Self::init_config(&env, &admin);
        Self::init_default_oracles(&env, &oracle_addresses);

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "initialized"), &true);
    }

    fn init_config(env: &Env, admin: &Address) {
        env.storage()
            .instance()
            .set(&Symbol::new(env, "admin"), admin);

        let mut req_methods = Vec::<Symbol>::new(env);
        req_methods.push_back(Symbol::new(&env, "physical_inspection"));
        req_methods.push_back(Symbol::new(&env, "document_verification"));
        req_methods.push_back(Symbol::new(&env, "blockchain_audit"));

        let config = ValidationConfig {
            min_oracle_reputation: 70,
            max_proof_age: 86400 * 30,
            required_verification_methods: req_methods,
            insurance_required: true,
            min_insurance_coverage: 1000000,
            audit_required: true,
            multi_oracle_required: true,
            oracle_consensus_threshold: 75,
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
            .set(&Symbol::new(&env, "proof_count"), &0u64);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "attestation_count"), &0u64);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "dispute_count"), &0u64);
        env.storage().instance().set(
            &Symbol::new(&env, "proofs"),
            &Vec::<CustodyProof>::new(&env),
        );
        env.storage().instance().set(
            &Symbol::new(&env, "disputes"),
            &Map::<u64, DisputeRecord>::new(&env),
        );
        env.storage().instance().set(
            &Symbol::new(&env, "oracles"),
            &Map::<Address, OracleInfo>::new(&env),
        );
        env.storage().instance().set(
            &Symbol::new(&env, "custodian_addresses"),
            &Vec::<Address>::new(&env),
        );
        env.storage().instance().set(
            &Symbol::new(&env, "registered_assets"),
            &Map::<Address, AssetRegistration>::new(&env),
        );
        env.storage().instance().set(
            &Symbol::new(&env, "verification_configs"),
            &Map::<Symbol, VerificationTypeConfig>::new(&env),
        );
        env.storage().instance().set(
            &Symbol::new(&env, "insurance_integrations"),
            &Map::<Address, InsuranceIntegration>::new(&env),
        );
        env.storage().instance().set(
            &Symbol::new(&env, "audit_trail"),
            &Map::<Address, Vec<CustodyAuditEvent>>::new(&env),
        );
        env.storage().instance().set(
            &Symbol::new(&env, "performance_scores"),
            &Map::<Address, PerformanceScore>::new(&env),
        );
        env.storage().instance().set(
            &Symbol::new(&env, "custodian_metrics"),
            &Map::<Address, CustodianMetrics>::new(&env),
        );
        env.storage().instance().set(
            &Symbol::new(&env, "performance_weights"),
            &PerformanceWeights {
                accuracy: 50,
                timeliness: 25,
                thoroughness: 25,
            },
        );
    }

    fn init_default_oracles(env: &Env, oracle_addresses: &Vec<Address>) {
        for oracle_addr in oracle_addresses.iter() {
            Self::put_oracle(
                env.clone(),
                oracle_addr.clone(),
                Symbol::new(&env, "Default"),
                Symbol::new(&env, "US"),
            );
        }

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
            panic_with_error!(env, CustodyError::StorageOutdated);
        }
    }

    // ── Issue #166: audit trail ──────────────────────────────────────────────

    fn append_audit_event(
        env: &Env,
        event_type: &Symbol,
        actor: &Address,
        asset_id: &Address,
        details_hash: &BytesN<32>,
    ) {
        let event = CustodyAuditEvent {
            event_type: event_type.clone(),
            timestamp: env.ledger().timestamp(),
            actor: actor.clone(),
            asset_id: asset_id.clone(),
            details_hash: details_hash.clone(),
        };

        let mut trail: Map<Address, Vec<CustodyAuditEvent>> = env
            .storage()
            .instance()
            .get(&Symbol::new(env, "audit_trail"))
            .unwrap_or_else(|| Map::new(env));

        let mut events = trail.get(asset_id.clone()).unwrap_or_else(|| Vec::new(env));
        events.push_back(event.clone());
        trail.set(asset_id.clone(), events);
        env.storage()
            .instance()
            .set(&Symbol::new(env, "audit_trail"), &trail);

        env.events().publish(
            (Symbol::new(env, "custody_audit"), event_type.clone()),
            (
                event.timestamp,
                actor.clone(),
                asset_id.clone(),
                details_hash.clone(),
            ),
        );
    }

    // ── Issue #167: performance scoring ─────────────────────────────────────

    fn get_performance_weights(env: &Env) -> PerformanceWeights {
        env.storage()
            .instance()
            .get(&Symbol::new(env, "performance_weights"))
            .unwrap_or(PerformanceWeights {
                accuracy: 50,
                timeliness: 25,
                thoroughness: 25,
            })
    }

    fn performance_metrics(env: &Env, custodian: &Address) -> CustodianMetrics {
        let metrics: Map<Address, CustodianMetrics> = env
            .storage()
            .instance()
            .get(&Symbol::new(env, "custodian_metrics"))
            .unwrap_or_else(|| Map::new(env));

        metrics.get(custodian.clone()).unwrap_or(CustodianMetrics {
            attestations_scored: 0,
            total_latency: 0,
            methods_evidenced: 0,
            methods_possible: 0,
        })
    }

    fn attestation_thoroughness(env: &Env, attestation: &CustodyAttestation) -> (u64, u64) {
        let configs: Map<Symbol, VerificationTypeConfig> = env
            .storage()
            .instance()
            .get(&Symbol::new(env, "verification_configs"))
            .unwrap_or_else(|| Map::new(env));

        let (sig_required, legal_required, audit_required) =
            match configs.get(attestation.verification_type.clone()) {
                Some(cfg) => (
                    if cfg.multi_sig_required {
                        cfg.sig_threshold
                    } else {
                        0
                    },
                    cfg.legal_verification_required,
                    cfg.audit_required,
                ),
                None => (0u32, false, false),
            };

        let zero = BytesN::from_array(env, &[0u8; 32]);
        let mut used: u64 = 1; // baseline proof of possession
        used += attestation.multi_sig_signatures.len() as u64;
        if attestation.legal_title_hash != zero {
            used += 1;
        }
        if attestation.audit_report_hash != zero {
            used += 1;
        }

        let required: u64 = 1
            + sig_required as u64
            + if legal_required { 1 } else { 0 }
            + if audit_required { 1 } else { 0 };
        let required = required.max(1);

        (used.min(required), required)
    }

    fn compute_performance(env: &Env, custodian: &CustodianRegistry) -> PerformanceScore {
        let weights = Self::get_performance_weights(env);
        let metrics = Self::performance_metrics(env, &custodian.custodian_address);

        let accuracy: u32 = if custodian.total_attestations == 0 {
            100
        } else {
            let good = custodian
                .total_attestations
                .saturating_sub(custodian.failed_disputes);
            ((good * 100) / custodian.total_attestations) as u32
        };

        let timeliness: u32 = if metrics.attestations_scored == 0 {
            100
        } else {
            let avg = metrics.total_latency / metrics.attestations_scored;
            if avg == 0 {
                100
            } else {
                (((EXPECTED_VERIFICATION_TIME * 100) / avg).min(100)) as u32
            }
        };

        let thoroughness: u32 = if metrics.methods_possible == 0 {
            100
        } else {
            (((metrics.methods_evidenced * 100) / metrics.methods_possible).min(100)) as u32
        };

        let total_weight = weights.accuracy + weights.timeliness + weights.thoroughness;
        let total_score = if total_weight == 0 {
            0
        } else {
            (accuracy * weights.accuracy
                + timeliness * weights.timeliness
                + thoroughness * weights.thoroughness)
                / total_weight
        };

        PerformanceScore {
            accuracy,
            timeliness,
            thoroughness,
            accuracy_weight: weights.accuracy,
            timeliness_weight: weights.timeliness,
            thoroughness_weight: weights.thoroughness,
            total_score,
        }
    }

    fn refresh_performance_score(env: &Env, custodian_address: &Address) {
        if let Some(custodian) = Self::read_custodian(env, custodian_address) {
            let score = Self::compute_performance(env, &custodian);
            let mut scores: Map<Address, PerformanceScore> = env
                .storage()
                .instance()
                .get(&Symbol::new(env, "performance_scores"))
                .unwrap_or_else(|| Map::new(env));
            scores.set(custodian_address.clone(), score);
            env.storage()
                .instance()
                .set(&Symbol::new(env, "performance_scores"), &scores);
        }
    }

    fn record_attestation_metrics(
        env: &Env,
        custodian: &Address,
        attestation: &CustodyAttestation,
    ) {
        let mut metrics: Map<Address, CustodianMetrics> = env
            .storage()
            .instance()
            .get(&Symbol::new(env, "custodian_metrics"))
            .unwrap_or_else(|| Map::new(env));

        let mut entry = metrics.get(custodian.clone()).unwrap_or(CustodianMetrics {
            attestations_scored: 0,
            total_latency: 0,
            methods_evidenced: 0,
            methods_possible: 0,
        });

        let latency = env
            .ledger()
            .timestamp()
            .saturating_sub(attestation.timestamp);
        let (used, required) = Self::attestation_thoroughness(env, attestation);

        entry.attestations_scored += 1;
        entry.total_latency += latency;
        entry.methods_evidenced += used;
        entry.methods_possible += required;

        metrics.set(custodian.clone(), entry);
        env.storage()
            .instance()
            .set(&Symbol::new(env, "custodian_metrics"), &metrics);
    }

    pub fn migrate(env: Env, auth: Address) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "admin"))
            .unwrap_or_else(|| {
                panic_with_error!(&env, CustodyError::ValidatorNotInitialized);
            });

        assert_admin(&env, &auth, &admin);

        let ver = Self::read_version(&env);
        if ver >= STORAGE_VERSION {
            panic_with_error!(&env, CustodyError::AlreadyAtLatestVersion);
        }

        let mut current = ver;
        while current < STORAGE_VERSION {
            current += 1;
        }

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "version"), &STORAGE_VERSION);
    }

    pub fn register_oracle(
        env: Env,
        auth: Address,
        oracle_address: Address,
        name: Symbol,
        jurisdiction: Symbol,
    ) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let registered_oracle = oracle_address.clone();
        Self::put_oracle(env.clone(), oracle_address, name, jurisdiction);

        // Issue #166: record the registration in the audit trail.
        Self::append_audit_event(
            &env,
            &Symbol::new(&env, "registered"),
            &auth,
            &registered_oracle,
            &BytesN::from_array(&env, &[0u8; 32]),
        );
    }

    pub fn register_custodian(
        env: Env,
        auth: Address,
        custodian_address: Address,
        name: Symbol,
        jurisdiction: Symbol,
        license_number: Symbol,
        verification_types: Vec<Symbol>,
        bond_required: i128,
        insurance_provider: Symbol,
    ) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        if bond_required < 0 {
            panic_with_error!(&env, CustodyError::InvalidParameters);
        }

        let custodian = CustodianRegistry {
            custodian_address: custodian_address.clone(),
            name,
            jurisdiction,
            license_number,
            reputation_score: 80,
            verification_types,
            is_active: true,
            total_attestations: 0,
            successful_disputes: 0,
            failed_disputes: 0,
            bond_required,
            insurance_provider,
        };

        Self::write_custodian(&env, &custodian_address, &custodian);

        let mut custodian_addresses: Vec<Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "custodian_addresses"))
            .unwrap_or(Vec::new(&env));

        if !custodian_addresses.contains(&custodian_address) {
            custodian_addresses.push_back(custodian_address.clone());
            env.storage().instance().set(
                &Symbol::new(&env, "custodian_addresses"),
                &custodian_addresses,
            );
        }

        // Issue #166: record the registration in the audit trail.
        Self::append_audit_event(
            &env,
            &Symbol::new(&env, "registered"),
            &auth,
            &custodian_address,
            &BytesN::from_array(&env, &[0u8; 32]),
        );
    }

    pub fn setup_verification_types(env: Env, auth: Address) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let mut verification_configs: Map<Symbol, VerificationTypeConfig> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "verification_configs"))
            .unwrap_or(Map::new(&env));

        verification_configs.set(
            Symbol::new(&env, "real_estate"),
            Self::get_real_estate_config(&env),
        );
        verification_configs.set(
            Symbol::new(&env, "precious_metals"),
            Self::get_metals_config(&env),
        );
        verification_configs.set(
            Symbol::new(&env, "art_collectibles"),
            Self::get_art_config(&env),
        );
        verification_configs.set(
            Symbol::new(&env, "commodities"),
            Self::get_commodities_config(&env),
        );
        verification_configs.set(Symbol::new(&env, "invoice"), Self::get_invoice_config(&env));

        env.storage().instance().set(
            &Symbol::new(&env, "verification_configs"),
            &verification_configs,
        );
    }

    fn get_real_estate_config(env: &Env) -> VerificationTypeConfig {
        let mut real_estate_docs = Vec::<Symbol>::new(env);
        real_estate_docs.push_back(Symbol::new(&env, "property_deed"));
        real_estate_docs.push_back(Symbol::new(&env, "title_insurance"));
        real_estate_docs.push_back(Symbol::new(&env, "rental_income_proof"));
        real_estate_docs.push_back(Symbol::new(&env, "inspection_report"));

        VerificationTypeConfig {
            verification_type: Symbol::new(&env, "real_estate"),
            required_documents: real_estate_docs,
            verification_frequency: 86400 * 7, // weekly
            multi_sig_required: true,
            sig_threshold: 3,
            insurance_required: true,
            min_insurance_coverage: 1000000,
            iot_monitoring_required: false,
            satellite_verification: true,
            legal_verification_required: true,
            audit_required: true,
        }
    }

    fn get_metals_config(env: &Env) -> VerificationTypeConfig {
        let mut metals_docs = Vec::<Symbol>::new(env);
        metals_docs.push_back(Symbol::new(&env, "vault_audit_cert"));
        metals_docs.push_back(Symbol::new(&env, "purity_assay"));
        metals_docs.push_back(Symbol::new(&env, "weight_verification"));
        metals_docs.push_back(Symbol::new(&env, "chain_of_custody"));

        VerificationTypeConfig {
            verification_type: Symbol::new(&env, "precious_metals"),
            required_documents: metals_docs,
            verification_frequency: 86400, // daily
            multi_sig_required: true,
            sig_threshold: 2,
            insurance_required: true,
            min_insurance_coverage: 500000,
            iot_monitoring_required: true,
            satellite_verification: false,
            legal_verification_required: false,
            audit_required: true,
        }
    }

    fn get_art_config(env: &Env) -> VerificationTypeConfig {
        let mut art_docs = Vec::<Symbol>::new(env);
        art_docs.push_back(Symbol::new(&env, "provenance_docs"));
        art_docs.push_back(Symbol::new(&env, "condition_report"));
        art_docs.push_back(Symbol::new(&env, "insurance_appraisal"));
        art_docs.push_back(Symbol::new(&env, "exhibition_history"));

        VerificationTypeConfig {
            verification_type: Symbol::new(&env, "art_collectibles"),
            required_documents: art_docs,
            verification_frequency: 86400 * 30, // monthly
            multi_sig_required: true,
            sig_threshold: 3,
            insurance_required: true,
            min_insurance_coverage: 250000,
            iot_monitoring_required: false,
            satellite_verification: false,
            legal_verification_required: true,
            audit_required: true,
        }
    }

    fn get_commodities_config(env: &Env) -> VerificationTypeConfig {
        let mut commodities_docs = Vec::<Symbol>::new(env);
        commodities_docs.push_back(Symbol::new(&env, "warehouse_receipt"));
        commodities_docs.push_back(Symbol::new(&env, "quality_grading"));
        commodities_docs.push_back(Symbol::new(&env, "environmental_cert"));

        VerificationTypeConfig {
            verification_type: Symbol::new(&env, "commodities"),
            required_documents: commodities_docs,
            verification_frequency: 86400 * 3, // every 3 days
            multi_sig_required: false,
            sig_threshold: 1,
            insurance_required: false,
            min_insurance_coverage: 100000,
            iot_monitoring_required: true,
            satellite_verification: false,
            legal_verification_required: false,
            audit_required: false,
        }
    }

    fn get_invoice_config(env: &Env) -> VerificationTypeConfig {
        let mut invoice_docs = Vec::<Symbol>::new(env);
        invoice_docs.push_back(Symbol::new(&env, "debtor_confirmation"));
        invoice_docs.push_back(Symbol::new(&env, "payment_history"));
        invoice_docs.push_back(Symbol::new(&env, "credit_insurance"));

        VerificationTypeConfig {
            verification_type: Symbol::new(&env, "invoice"),
            required_documents: invoice_docs,
            verification_frequency: 86400 * 14, // biweekly
            multi_sig_required: false,
            sig_threshold: 1,
            insurance_required: true,
            min_insurance_coverage: 75000,
            iot_monitoring_required: false,
            satellite_verification: false,
            legal_verification_required: true,
            audit_required: true,
        }
    }

    /// Caps a dispute penalty at the bond posted for the dispute and records
    /// the resulting slash. Token movement itself is settled by the caller /
    /// off-chain bond custodian, which is why only the capped amount is
    /// returned and emitted here.
    fn apply_bond_slash(env: &Env, dispute: &DisputeRecord, penalty_amount: i128) -> i128 {
        let slashed = penalty_amount.max(0).min(dispute.bond_amount);
        env.events().publish(
            (Symbol::new(env, "bond_slashed"), dispute.custodian.clone()),
            (slashed, dispute.dispute_id),
        );
        slashed
    }

    pub fn resolve_dispute(
        env: Env,
        auth: Address,
        dispute_id: u64,
        resolution: Symbol,
        penalty_amount: i128,
    ) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let mut disputes: Map<u64, DisputeRecord> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "disputes"))
            .unwrap_or(Map::new(&env));

        let mut dispute = disputes
            .get(dispute_id)
            .ok_or(CustodyError::DisputeNotFound)
            .unwrap();

        if dispute.status != Symbol::new(&env, "pending") {
            panic_with_error!(&env, CustodyError::InvalidDisputeStatus);
        }

        dispute.status = if resolution == Symbol::new(&env, "upheld") {
            Symbol::new(&env, "resolved_upheld")
        } else if resolution == Symbol::new(&env, "rejected") {
            Symbol::new(&env, "resolved_rejected")
        } else {
            Symbol::new(&env, "resolved_settled")
        };

        dispute.resolved_at = env.ledger().timestamp();
        dispute.resolution = resolution.clone();

        if resolution == Symbol::new(&env, "upheld") {
            // Issue #158: an upheld dispute automatically slashes the custodian's
            // bond (capped at the total posted bond) and redistributes it.
            // The challenger's own bond is returned since they prevailed.
            dispute.bond_returned = true;
            let slashed = Self::apply_bond_slash(&env, &dispute, penalty_amount);
            dispute.penalty_applied = slashed > 0;
            dispute.penalty_amount = slashed;
        } else {
            // Rejected / settled disputes never slash the custodian's bond.
            dispute.bond_returned = false;
            dispute.penalty_applied = false;
            dispute.penalty_amount = 0;
            Self::update_custodian_dispute_stats(env.clone(), dispute.custodian.clone(), false);
        }

        disputes.set(dispute_id, dispute.clone());
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "disputes"), &disputes);

        // Issue #166 + #167: audit the resolution and recompute the score.
        let resolved_asset = Self::read_attestation(&env, &dispute.attestation_id)
            .map(|attestation| attestation.asset_id)
            .unwrap_or_else(|| dispute.custodian.clone());
        Self::append_audit_event(
            &env,
            &Symbol::new(&env, "resolved"),
            &auth,
            &resolved_asset,
            &BytesN::from_array(&env, &[0u8; 32]),
        );
        Self::refresh_performance_score(&env, &dispute.custodian);

        env.events().publish(
            (
                Symbol::new(&env, "dispute_resolved"),
                dispute.custodian.clone(),
            ),
            (
                dispute.dispute_id,
                resolution,
                dispute.challenger,
                dispute.custodian,
                env.ledger().timestamp(),
            ),
        );
    }

    fn update_custodian_stats(env: Env, custodian_address: Address) {
        if let Some(mut custodian) = Self::read_custodian(&env, &custodian_address) {
            custodian.total_attestations += 1;
            if custodian.total_attestations % 10 == 0 {
                custodian.reputation_score = (custodian.reputation_score + 1).min(100);
            }
            Self::write_custodian(&env, &custodian_address, &custodian);
        }
    }

    fn update_custodian_dispute_stats(env: Env, custodian_address: Address, dispute_lost: bool) {
        if let Some(mut custodian) = Self::read_custodian(&env, &custodian_address) {
            if dispute_lost {
                custodian.failed_disputes += 1;
                custodian.reputation_score = custodian.reputation_score.saturating_sub(5);
                if custodian.reputation_score < 50 {
                    custodian.is_active = false;
                }
            } else {
                custodian.successful_disputes += 1;
                custodian.reputation_score = (custodian.reputation_score + 2).min(100);
            }
            Self::write_custodian(&env, &custodian_address, &custodian);
        }
    }

    pub fn submit_attestation(env: Env, attestation: CustodyAttestation) -> u64 {
        Self::check_version(&env);

        if attestation.value < 0 {
            panic_with_error!(&env, CustodyError::InvalidParameters);
        }

        // Issue #162: a custodian may only attest for the asset types it has
        // been authorised for by the registry.
        let custodian_info =
            Self::read_custodian(&env, &attestation.custodian).unwrap_or_else(|| {
                panic_with_error!(&env, CustodyError::CustodianNotWhitelisted);
            });
        if !custodian_info.is_active {
            panic_with_error!(&env, CustodyError::CustodianNotWhitelisted);
        }
        if !custodian_info
            .verification_types
            .contains(&attestation.verification_type)
        {
            panic_with_error!(&env, CustodyError::InvalidVerificationType);
        }

        let registered_assets: Map<Address, AssetRegistration> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "registered_assets"))
            .unwrap_or_else(|| Map::new(&env));
        if let Some(registration) = registered_assets.get(attestation.asset_id.clone()) {
            if !custodian_info
                .verification_types
                .contains(&registration.asset_type)
            {
                panic_with_error!(&env, CustodyError::InvalidVerificationType);
            }
        }

        if !Self::verify_attestation(&env, &attestation) {
            panic_with_error!(&env, CustodyError::InvalidAttestation);
        }

        let attestation_count: u64 = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "attestation_count"))
            .unwrap_or(0u64);

        let attestation_id = attestation_count + 1;

        let mut valid_attestation = attestation;
        valid_attestation.is_valid = true;
        // Honour a caller-supplied expiry, but never store one longer than the
        // default 30-day validity window.
        let default_expiry = env.ledger().timestamp() + 86400 * 30;
        if valid_attestation.expires_at > default_expiry {
            valid_attestation.expires_at = default_expiry;
        }

        let custodian = valid_attestation.custodian.clone();
        let asset_id = valid_attestation.asset_id.clone();
        let value = valid_attestation.value;

        Self::write_attestation(&env, &attestation_id, &valid_attestation);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "attestation_count"), &attestation_id);

        Self::update_custodian_stats(env.clone(), custodian.clone());

        // Issue #167: refresh the weighted performance score for this custodian.
        Self::record_attestation_metrics(&env, &custodian, &valid_attestation);
        Self::refresh_performance_score(&env, &custodian);

        // Issue #166: append an immutable audit-trail entry.
        Self::append_audit_event(
            &env,
            &Symbol::new(&env, "attestation_submitted"),
            &custodian,
            &asset_id,
            &valid_attestation.proof_hash,
        );

        env.events().publish(
            (Symbol::new(&env, "attestation_submitted"), asset_id),
            (attestation_id, custodian, value, env.ledger().timestamp()),
        );

        attestation_id
    }

    /// Checks shared by every attestation path (active custodian, authorised
    /// verification type, insurance requirement and expiry). The multi-signature
    /// requirement is deliberately excluded: `submit_attestation` enforces the
    /// verification-type signature count while `submit_multisig_attestation`
    /// enforces the custodian's M-of-N policy (issue #155).
    fn verify_attestation_basics(env: &Env, attestation: &CustodyAttestation) -> bool {
        let custodian_info = match Self::read_custodian(env, &attestation.custodian) {
            Some(info) => info,
            None => return false,
        };

        if !custodian_info.is_active {
            return false;
        }

        if !custodian_info
            .verification_types
            .contains(&attestation.verification_type)
        {
            return false;
        }

        let verification_configs: Map<Symbol, VerificationTypeConfig> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "verification_configs"))
            .unwrap_or(Map::new(&env));

        if let Some(config) = verification_configs.get(attestation.verification_type.clone()) {
            if config.insurance_required
                && attestation.insurance_status == Symbol::new(&env, "uninsured")
            {
                return false;
            }
        }

        let current_time = env.ledger().timestamp();
        if current_time > attestation.expires_at {
            return false;
        }

        true
    }

    fn verify_attestation(env: &Env, attestation: &CustodyAttestation) -> bool {
        if !Self::verify_attestation_basics(env, attestation) {
            return false;
        }

        let verification_configs: Map<Symbol, VerificationTypeConfig> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "verification_configs"))
            .unwrap_or(Map::new(&env));

        if let Some(config) = verification_configs.get(attestation.verification_type.clone()) {
            if config.multi_sig_required
                && (attestation.multi_sig_signatures.len() as u32) < config.sig_threshold
            {
                return false;
            }
        }

        true
    }

    pub fn dispute_attestation(
        env: Env,
        attestation_id: u64,
        challenger: Address,
        reason: Symbol,
        bond_amount: i128,
        evidence_hash: BytesN<32>,
    ) -> u64 {
        Self::check_version(&env);

        let attestations: Map<u64, CustodyAttestation> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "attestations"))
            .unwrap_or(Map::new(&env));

        let attestation = Self::read_attestation(&env, &attestation_id)
            .ok_or(CustodyError::DisputeNotFound)
            .unwrap();

        let custodian_info = Self::read_custodian(&env, &attestation.custodian)
            .ok_or(CustodyError::CustodianNotWhitelisted)
            .unwrap();

        if bond_amount < custodian_info.bond_required {
            panic_with_error!(&env, CustodyError::InsufficientBond);
        }

        let disputes: Map<u64, DisputeRecord> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "disputes"))
            .unwrap_or(Map::new(&env));

        let mut inspected = 0u32;
        for dispute in disputes.iter() {
            if inspected >= MAX_ITERATIONS {
                break;
            }
            inspected += 1;
            if dispute.1.attestation_id == attestation_id
                && dispute.1.status == Symbol::new(&env, "pending")
            {
                panic_with_error!(&env, CustodyError::DisputeAlreadyExists);
            }
        }

        // Issue #166: record the filed dispute in the audit trail.
        Self::append_audit_event(
            &env,
            &Symbol::new(&env, "dispute_filed"),
            &challenger,
            &attestation.asset_id,
            &evidence_hash,
        );

        let dispute_count: u64 = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "dispute_count"))
            .unwrap_or(0u64);

        let dispute_id = dispute_count + 1;

        let dispute = DisputeRecord {
            dispute_id,
            attestation_id,
            challenger: challenger.clone(),
            custodian: attestation.custodian.clone(),
            reason,
            bond_amount,
            evidence_hash,
            status: Symbol::new(&env, "pending"),
            created_at: env.ledger().timestamp(),
            resolved_at: 0,
            resolution: Symbol::new(&env, "none"),
            bond_returned: false,
            penalty_applied: false,
            penalty_amount: 0,
        };

        let mut updated_disputes = disputes;
        updated_disputes.set(dispute_id, dispute.clone());
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "disputes"), &updated_disputes);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "dispute_count"), &dispute_id);

        env.events().publish(
            (Symbol::new(&env, "dispute_initiated"), attestation.asset_id),
            (
                dispute_id,
                challenger,
                attestation.custodian,
                env.ledger().timestamp(),
            ),
        );

        dispute_id
    }

    pub fn validate_proof(env: Env, proof: CustodyProof) -> bool {
        let config: ValidationConfig = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "config"))
            .unwrap_or_else(|| {
                panic_with_error!(&env, CustodyError::ConfigNotFound);
            });

        let registered_assets: Map<Address, AssetRegistration> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "registered_assets"))
            .unwrap_or(Map::new(&env));

        if registered_assets.get(proof.asset_address.clone()).is_none() {
            return false;
        }

        let oracles: Map<Address, OracleInfo> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "oracles"))
            .unwrap_or(Map::new(&env));

        match oracles.get(proof.custody_provider.clone()) {
            Some(oracle_info) => {
                if oracle_info.reputation_score < config.min_oracle_reputation {
                    return false;
                }
                if !oracle_info.is_active {
                    return false;
                }
            }
            None => return false,
        }

        let current_time = env.ledger().timestamp();
        if current_time - proof.verification_timestamp > config.max_proof_age {
            return false;
        }

        if current_time > proof.expiry_timestamp {
            return false;
        }

        if config.insurance_required && proof.insurance_coverage < config.min_insurance_coverage {
            return false;
        }

        if config.audit_required && proof.audit_report_hash == BytesN::from_array(&env, &[0; 32]) {
            return false;
        }

        if config.multi_oracle_required
            && !Self::verify_oracle_signatures(env.clone(), proof.clone())
        {
            return false;
        }

        true
    }

    fn verify_oracle_signatures(env: Env, proof: CustodyProof) -> bool {
        let config: ValidationConfig = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "config"))
            .unwrap_or_else(|| {
                panic_with_error!(&env, CustodyError::ConfigNotFound);
            });

        let valid_signatures = proof.oracle_signatures.len();
        let total_signatures = proof.oracle_signatures.len();

        if total_signatures == 0 {
            return false;
        }

        let consensus_percentage = (valid_signatures * 100) / total_signatures as u32;
        consensus_percentage >= config.oracle_consensus_threshold
    }

    fn update_oracle_stats(env: Env, oracle_address: Address) {
        let mut oracles: Map<Address, OracleInfo> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "oracles"))
            .unwrap_or(Map::new(&env));

        if let Some(mut oracle_info) = oracles.get(oracle_address.clone()) {
            oracle_info.last_verification = env.ledger().timestamp();
            oracle_info.total_verifications += 1;
            if oracle_info.total_verifications % 10 == 0 {
                oracle_info.reputation_score = (oracle_info.reputation_score + 1).min(100);
            }
            oracles.set(oracle_address, oracle_info);
            env.storage()
                .instance()
                .set(&Symbol::new(&env, "oracles"), &oracles);
        }
    }

    pub fn get_attestation(env: Env, attestation_id: u64) -> CustodyAttestation {
        let mut attestation = Self::read_attestation(&env, &attestation_id)
            .unwrap_or_else(|| panic_with_error!(&env, CustodyError::AttestationNotFound));

        let now = env.ledger().timestamp();
        if now > attestation.expires_at {
            if attestation.is_valid {
                attestation.is_valid = false;
                Self::write_attestation(&env, &attestation_id, &attestation);
                env.events().publish(
                    (
                        Symbol::new(&env, "attestation_expired"),
                        attestation.asset_id.clone(),
                    ),
                    (attestation_id, now),
                );
            }
        }
        attestation
    }

    pub fn is_attestation_valid(env: Env, attestation_id: u64) -> bool {
        if let Some(attestation) = Self::read_attestation(&env, &attestation_id) {
            attestation.is_valid && env.ledger().timestamp() <= attestation.expires_at
        } else {
            false
        }
    }

    pub fn invalidate_attestation(env: Env, auth: Address, attestation_id: u64) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let mut attestation = Self::read_attestation(&env, &attestation_id)
            .unwrap_or_else(|| panic_with_error!(&env, CustodyError::AttestationNotFound));

        attestation.is_valid = false;
        Self::write_attestation(&env, &attestation_id, &attestation);

        // Issue #166: append an immutable audit-trail entry.
        Self::append_audit_event(
            &env,
            &Symbol::new(&env, "invalidated"),
            &auth,
            &attestation.asset_id,
            &attestation.proof_hash,
        );

        env.events().publish(
            (
                Symbol::new(&env, "attestation_invalidated"),
                attestation.asset_id.clone(),
            ),
            (attestation_id, auth, env.ledger().timestamp()),
        );
    }

    pub fn get_latest_attestation(env: Env, asset_id: Address) -> Option<CustodyAttestation> {
        let attestation_count: u64 = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "attestation_count"))
            .unwrap_or(0u64);

        let mut latest_attestation: Option<CustodyAttestation> = None;
        let mut latest_timestamp = 0u64;

        let start = if attestation_count > MAX_ITERATIONS as u64 {
            attestation_count - MAX_ITERATIONS as u64
        } else {
            1
        };

        for id in start..=attestation_count {
            if let Some(att) = Self::read_attestation(&env, &id) {
                if att.asset_id == asset_id && att.is_valid && att.timestamp > latest_timestamp {
                    latest_timestamp = att.timestamp;
                    latest_attestation = Some(att);
                }
            }
        }

        latest_attestation
    }

    pub fn get_dispute(env: Env, dispute_id: u64) -> DisputeRecord {
        let disputes: Map<u64, DisputeRecord> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "disputes"))
            .unwrap_or(Map::new(&env));

        disputes.get(dispute_id).unwrap_or_else(|| {
            panic_with_error!(&env, CustodyError::DisputeNotFound);
        })
    }

    pub fn get_custodian_info(env: Env, custodian_address: Address) -> CustodianRegistry {
        Self::read_custodian(&env, &custodian_address)
            .unwrap_or_else(|| panic_with_error!(&env, CustodyError::CustodianNotFound))
    }

    pub fn list_active_custodians(env: Env) -> Vec<CustodianRegistry> {
        let custodian_addresses: Vec<Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "custodian_addresses"))
            .unwrap_or(Vec::new(&env));

        let mut active_custodians = Vec::<CustodianRegistry>::new(&env);
        let mut processed = 0u32;
        for addr in custodian_addresses.iter() {
            if processed >= MAX_ITERATIONS {
                break;
            }
            processed += 1;
            if let Some(custodian) = Self::read_custodian(&env, &addr) {
                if custodian.is_active {
                    active_custodians.push_back(custodian);
                }
            }
        }

        active_custodians
    }

    pub fn get_verification_config(env: Env, verification_type: Symbol) -> VerificationTypeConfig {
        let verification_configs: Map<Symbol, VerificationTypeConfig> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "verification_configs"))
            .unwrap_or(Map::new(&env));

        verification_configs
            .get(verification_type)
            .unwrap_or_else(|| {
                panic_with_error!(&env, CustodyError::InvalidVerificationType);
            })
    }

    pub fn trigger_insurance_claim(
        env: Env,
        auth: Address,
        asset_id: Address,
        claim_reason: Symbol,
        evidence_hash: BytesN<32>,
    ) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let insurance_integrations: Map<Address, InsuranceIntegration> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "insurance_integrations"))
            .unwrap_or(Map::new(&env));

        if let Some(insurance) = insurance_integrations.get(asset_id.clone()) {
            if !insurance.claim_auto_trigger {
                panic_with_error!(&env, CustodyError::InsuranceClaimFailed);
            }

            if env.ledger().timestamp() > insurance.valid_until {
                panic_with_error!(&env, CustodyError::InsuranceClaimFailed);
            }

            // Issue #166: record the insurance claim in the audit trail.
            Self::append_audit_event(
                &env,
                &Symbol::new(&env, "insurance_claimed"),
                &auth,
                &asset_id,
                &evidence_hash,
            );

            env.events().publish(
                (Symbol::new(&env, "insurance_claim_triggered"), asset_id),
                (
                    insurance.provider,
                    claim_reason,
                    evidence_hash,
                    env.ledger().timestamp(),
                ),
            );
        } else {
            panic_with_error!(&env, CustodyError::InsuranceClaimFailed);
        }
    }

    pub fn setup_insurance_integration(
        env: Env,
        auth: Address,
        asset_id: Address,
        insurance: InsuranceIntegration,
    ) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let mut insurance_integrations: Map<Address, InsuranceIntegration> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "insurance_integrations"))
            .unwrap_or(Map::new(&env));

        insurance_integrations.set(asset_id, insurance);
        env.storage().instance().set(
            &Symbol::new(&env, "insurance_integrations"),
            &insurance_integrations,
        );
    }

    pub fn get_custody_alerts(env: Env) -> Vec<(Address, Symbol)> {
        let attestation_count: u64 = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "attestation_count"))
            .unwrap_or(0u64);

        let mut alerts = Vec::<(Address, Symbol)>::new(&env);
        let current_time = env.ledger().timestamp();

        let start = if attestation_count > MAX_ITERATIONS as u64 {
            attestation_count - MAX_ITERATIONS as u64
        } else {
            1
        };

        for id in start..=attestation_count {
            if let Some(attestation) = Self::read_attestation(&env, &id) {
                if !attestation.is_valid {
                    alerts.push_back((
                        attestation.asset_id.clone(),
                        Symbol::new(&env, "invalid_attestation"),
                    ));
                } else if current_time > attestation.expires_at {
                    alerts.push_back((
                        attestation.asset_id.clone(),
                        Symbol::new(&env, "attestation_expired"),
                    ));
                } else if current_time > attestation.expires_at - 86400 * 7 {
                    alerts.push_back((
                        attestation.asset_id.clone(),
                        Symbol::new(&env, "attestation_expiring_soon"),
                    ));
                }
            }
        }

        alerts
    }

    fn apply_oracle_decay(env: &Env, mut oracle_info: OracleInfo) -> OracleInfo {
        let now = env.ledger().timestamp();
        if oracle_info.last_verification > 0 && now > oracle_info.last_verification {
            let elapsed = now - oracle_info.last_verification;
            let decay_periods = elapsed / (7 * 86400);
            if decay_periods > 0 {
                let decay = decay_periods as u32;
                oracle_info.reputation_score =
                    oracle_info.reputation_score.saturating_sub(decay).max(10);
            }
        }
        oracle_info
    }

    pub fn get_oracle_info(env: Env, oracle_address: Address) -> OracleInfo {
        let oracles: Map<Address, OracleInfo> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "oracles"))
            .unwrap_or(Map::new(&env));

        let oracle_info = oracles.get(oracle_address).unwrap_or_else(|| {
            panic_with_error!(&env, CustodyError::OracleNotFound);
        });
        Self::apply_oracle_decay(&env, oracle_info)
    }

    pub fn list_active_oracles(env: Env) -> Vec<OracleInfo> {
        let oracles: Map<Address, OracleInfo> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "oracles"))
            .unwrap_or(Map::new(&env));

        let mut active_oracles = Vec::<OracleInfo>::new(&env);
        let mut processed = 0u32;
        for (_, oracle_info) in oracles.iter() {
            if processed >= MAX_ITERATIONS {
                break;
            }
            processed += 1;
            if oracle_info.is_active {
                active_oracles.push_back(oracle_info.clone());
            }
        }

        active_oracles
    }

    pub fn update_oracle_status(env: Env, auth: Address, oracle_address: Address, is_active: bool) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let mut oracles: Map<Address, OracleInfo> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "oracles"))
            .unwrap_or(Map::new(&env));

        if let Some(mut oracle_info) = oracles.get(oracle_address.clone()) {
            oracle_info.is_active = is_active;
            oracles.set(oracle_address, oracle_info);
            env.storage()
                .instance()
                .set(&Symbol::new(&env, "oracles"), &oracles);
        }
    }

    pub fn update_oracle_reputation(
        env: Env,
        auth: Address,
        oracle_address: Address,
        reputation_score: u32,
    ) {
        crate::shared_admin::require_admin(&env, &auth);

        if reputation_score > 100 {
            panic_with_error!(&env, CustodyError::VerificationFailed);
        }

        Self::check_version(&env);

        let mut oracles: Map<Address, OracleInfo> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "oracles"))
            .unwrap_or(Map::new(&env));

        if let Some(mut oracle_info) = oracles.get(oracle_address.clone()) {
            oracle_info.reputation_score = reputation_score;
            oracle_info.last_verification = env.ledger().timestamp();
            oracles.set(oracle_address, oracle_info);
            env.storage()
                .instance()
                .set(&Symbol::new(&env, "oracles"), &oracles);
        }
    }

    pub fn get_asset_registration(env: Env, asset_address: Address) -> AssetRegistration {
        let registered_assets: Map<Address, AssetRegistration> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "registered_assets"))
            .unwrap_or(Map::new(&env));

        registered_assets.get(asset_address).unwrap_or_else(|| {
            panic_with_error!(&env, CustodyError::AssetNotRegistered);
        })
    }

    pub fn update_config(env: Env, auth: Address, config: ValidationConfig) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "config"), &config);
    }

    pub fn get_validation_stats(env: Env) -> Map<Symbol, u64> {
        let proofs: Vec<CustodyProof> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "proofs"))
            .unwrap_or(Vec::new(&env));

        let oracles: Map<Address, OracleInfo> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "oracles"))
            .unwrap_or(Map::new(&env));

        let mut stats = Map::<Symbol, u64>::new(&env);

        stats.set(Symbol::new(&env, "total_proofs"), proofs.len() as u64);
        stats.set(Symbol::new(&env, "total_oracles"), oracles.len() as u64);

        let mut valid_proofs = 0u64;
        let mut expired_proofs = 0u64;
        let current_time = env.ledger().timestamp();

        let mut processed = 0u32;
        for proof in proofs.iter() {
            if processed >= MAX_ITERATIONS {
                break;
            }
            processed += 1;
            if proof.is_valid && current_time <= proof.expiry_timestamp {
                valid_proofs += 1;
            } else if current_time > proof.expiry_timestamp {
                expired_proofs += 1;
            }
        }

        stats.set(Symbol::new(&env, "valid_proofs"), valid_proofs);
        stats.set(Symbol::new(&env, "expired_proofs"), expired_proofs);
        stats
    }

    // ── Issue #162: custodian verification-type management ────────────────

    /// Update the set of asset types a registered custodian is authorised to
    /// attest for. Admin-only.
    pub fn set_custodian_verification_types(
        env: Env,
        auth: Address,
        custodian_address: Address,
        verification_types: Vec<Symbol>,
    ) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let mut custodian = Self::read_custodian(&env, &custodian_address).unwrap_or_else(|| {
            panic_with_error!(&env, CustodyError::CustodianNotFound);
        });
        custodian.verification_types = verification_types;
        Self::write_custodian(&env, &custodian_address, &custodian);

        Self::append_audit_event(
            &env,
            &Symbol::new(&env, "registered"),
            &auth,
            &custodian_address,
            &BytesN::from_array(&env, &[0u8; 32]),
        );
    }

    // ── Issue #166: audit trail query ──────────────────────────────────────

    /// Return the audit-trail entries for `asset_id` whose timestamp falls in
    /// the inclusive `[start, end]` window. Pass `end = 0` for no upper bound.
    /// Work is bounded by `MAX_ITERATIONS` to keep gas predictable.
    pub fn get_audit_trail(
        env: Env,
        asset_id: Address,
        start: u64,
        end: u64,
    ) -> Vec<CustodyAuditEvent> {
        let effective_end = if end == 0 { u64::MAX } else { end };

        let trail: Map<Address, Vec<CustodyAuditEvent>> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "audit_trail"))
            .unwrap_or_else(|| Map::new(&env));

        let events = trail
            .get(asset_id.clone())
            .unwrap_or_else(|| Vec::new(&env));

        let mut result = Vec::<CustodyAuditEvent>::new(&env);
        let mut processed = 0u32;
        for event in events.iter() {
            if processed >= MAX_ITERATIONS {
                break;
            }
            processed += 1;
            if event.timestamp >= start && event.timestamp <= effective_end {
                result.push_back(event);
            }
        }

        result
    }

    // ── Issue #167: performance score queries / weight management ──────────

    /// Return the weighted performance score for a custodian. The score is
    /// recomputed from raw metrics whenever a stored value is unavailable.
    pub fn get_performance_score(env: Env, custodian_address: Address) -> PerformanceScore {
        let custodian = Self::read_custodian(&env, &custodian_address).unwrap_or_else(|| {
            panic_with_error!(&env, CustodyError::CustodianNotFound);
        });

        let scores: Map<Address, PerformanceScore> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "performance_scores"))
            .unwrap_or_else(|| Map::new(&env));

        if let Some(score) = scores.get(custodian_address.clone()) {
            return score;
        }

        Self::compute_performance(&env, &custodian)
    }

    /// Update the weights used to combine performance metrics. Admin-only.
    pub fn update_performance_weights(env: Env, auth: Address, weights: PerformanceWeights) {
        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        if weights.accuracy > 100 || weights.timeliness > 100 || weights.thoroughness > 100 {
            panic_with_error!(&env, CustodyError::InvalidParameters);
        }

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "performance_weights"), &weights);

        // Recompute cached scores so callers immediately observe the new weights.
        let custodian_addresses: Vec<Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "custodian_addresses"))
            .unwrap_or_else(|| Vec::new(&env));

        let mut processed = 0u32;
        for address in custodian_addresses.iter() {
            if processed >= MAX_ITERATIONS {
                break;
            }
            processed += 1;
            Self::refresh_performance_score(&env, &address);
        }
    }

    // ── Issue #168: paginated attestation listing ──────────────────────────

    /// Return up to `limit` attestations ordered by id, starting immediately
    /// after `cursor`. `limit` is clamped to `MAX_ITERATIONS`. The second tuple
    /// element is a continuation cursor, or `None` when the page is the last.
    pub fn get_attestations(
        env: Env,
        cursor: u64,
        limit: u32,
    ) -> (Vec<CustodyAttestation>, Option<u64>) {
        let attestation_count: u64 = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "attestation_count"))
            .unwrap_or(0u64);

        if cursor >= attestation_count {
            return (Vec::new(&env), None);
        }

        let capped_limit = if limit == 0 || limit > MAX_ITERATIONS {
            MAX_ITERATIONS
        } else {
            limit
        };

        let mut page = Vec::<CustodyAttestation>::new(&env);
        let mut processed = 0u32;
        let mut last_id = cursor;
        let mut id = cursor + 1;

        while id <= attestation_count && processed < capped_limit {
            if let Some(attestation) = Self::read_attestation(&env, &id) {
                page.push_back(attestation);
                last_id = id;
                processed += 1;
            }
            id += 1;
        }

        let next_cursor = if id <= attestation_count && processed > 0 {
            Some(last_id)
        } else {
            None
        };

        (page, next_cursor)
    }
}

// ── Issue #150: shared price-oracle helpers ──────────────────────────────────
//
// Both the custody validator (fair-value checks on attested collateral) and the
// secondary market (order validation) need to agree on when an oracle price is
// trustworthy. These helpers are the single source of truth for that policy.

/// Default maximum age of an oracle price before it is considered stale.
pub const PRICE_ORACLE_MAX_AGE_SECONDS: u64 = 3600;

/// True when `timestamp` is no older than `max_age_seconds` relative to `now`.
/// A timestamp in the future is never considered fresh.
pub fn is_oracle_price_fresh(timestamp: u64, now: u64, max_age_seconds: u64) -> bool {
    now >= timestamp && now - timestamp <= max_age_seconds
}

/// Absolute difference between `price` and `reference` expressed in basis
/// points of the reference. Returns 0 when the reference is not positive.
pub fn price_deviation_bps(price: i128, reference: i128) -> i128 {
    if reference <= 0 {
        return 0;
    }
    let deviation = if price >= reference {
        price - reference
    } else {
        reference - price
    };
    (deviation * 10000) / reference
}
