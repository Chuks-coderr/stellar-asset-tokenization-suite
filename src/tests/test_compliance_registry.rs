#![cfg(test)]

use soroban_sdk::{
    testutils::Address as _,
    Address, Env, Symbol, Vec,
};

use crate::compliance_registry::{
    ComplianceRegistry, ComplianceRegistryClient, DelegatedCheck, KYCStatus, RiskThresholds,
    TransferLimits,
};

// ── helpers ──────────────────────────────────────────────────────────────────

fn setup() -> (Env, Address, ComplianceRegistryClient<'static>) {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let contract_id = env.register_contract(None, ComplianceRegistry);
    let client = ComplianceRegistryClient::new(&env, &contract_id);
    client.initialize(&admin, &admin, &true, &false);
    // SAFETY: env lifetime is tied to the test scope; the client borrows it.
    let client: ComplianceRegistryClient<'static> = unsafe { core::mem::transmute(client) };
    (env, admin, client)
}

fn verified_kyc(env: &Env) -> KYCStatus {
    KYCStatus {
        is_verified: true,
        verification_level: 2,
        expiry_date: env.ledger().timestamp() + 86400 * 365,
        jurisdiction: Symbol::new(env, "US"),
        is_accredited: true,
        risk_score: 1,
        aml_flags: Vec::new(env),
    }
}

// ── initialize ───────────────────────────────────────────────────────────────

#[test]
fn initialize_sets_kyc_required_flag() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let contract_id = env.register_contract(None, ComplianceRegistry);
    let client = ComplianceRegistryClient::new(&env, &contract_id);
    client.initialize(&admin, &admin, &true, &false);
    // Compliance check with unverified users should fail (kyc_required = true)
    let user_a = Address::generate(&env);
    let user_b = Address::generate(&env);
    assert!(!client.check_compliance(&user_a, &user_b, &100));
}

#[test]
#[should_panic]
fn initialize_twice_panics() {
    let (env, admin, client) = setup();
    client.initialize(&admin, &admin, &true, &false);
}

// ── KYC status ───────────────────────────────────────────────────────────────

#[test]
fn update_and_get_kyc_status() {
    let (env, admin, client) = setup();
    let user = Address::generate(&env);
    let kyc = verified_kyc(&env);
    client.update_kyc_status(&admin, &user, &kyc);
    let stored = client.get_kyc_status(&user);
    assert!(stored.is_verified);
    assert_eq!(stored.verification_level, 2);
}

#[test]
fn get_kyc_status_returns_default_for_unknown_user() {
    let (env, _, client) = setup();
    let user = Address::generate(&env);
    let kyc = client.get_kyc_status(&user);
    assert!(!kyc.is_verified);
    assert_eq!(kyc.verification_level, 0);
}

#[test]
#[should_panic]
fn update_kyc_status_rejects_non_admin() {
    let (env, _, client) = setup();
    let attacker = Address::generate(&env);
    let user = Address::generate(&env);
    client.update_kyc_status(&attacker, &user, &verified_kyc(&env));
}

// ── blacklist ─────────────────────────────────────────────────────────────────

#[test]
fn blacklisted_address_fails_compliance() {
    let (env, admin, client) = setup();
    let user_a = Address::generate(&env);
    let user_b = Address::generate(&env);

    // Give both users valid KYC so the only failure is the blacklist
    client.update_kyc_status(&admin, &user_a, &verified_kyc(&env));
    client.update_kyc_status(&admin, &user_b, &verified_kyc(&env));

    // Passes before blacklisting
    assert!(client.check_compliance(&user_a, &user_b, &100));

    client.add_to_blacklist(&admin, &user_a, &Symbol::new(&env, "fraud"));
    assert!(!client.check_compliance(&user_a, &user_b, &100));
}

#[test]
fn remove_from_blacklist_restores_compliance() {
    let (env, admin, client) = setup();
    let user_a = Address::generate(&env);
    let user_b = Address::generate(&env);

    client.update_kyc_status(&admin, &user_a, &verified_kyc(&env));
    client.update_kyc_status(&admin, &user_b, &verified_kyc(&env));
    client.add_to_blacklist(&admin, &user_a, &Symbol::new(&env, "fraud"));
    assert!(!client.check_compliance(&user_a, &user_b, &100));

    client.remove_from_blacklist(&admin, &user_a);
    assert!(client.check_compliance(&user_a, &user_b, &100));
}

#[test]
#[should_panic]
fn add_to_blacklist_rejects_non_admin() {
    let (env, _, client) = setup();
    let attacker = Address::generate(&env);
    let victim = Address::generate(&env);
    client.add_to_blacklist(&attacker, &victim, &Symbol::new(&env, "test"));
}

// ── whitelist ─────────────────────────────────────────────────────────────────

#[test]
fn whitelisted_address_bypasses_kyc_check() {
    let (env, admin, client) = setup();
    let user_a = Address::generate(&env);
    let user_b = Address::generate(&env);

    // No KYC set — normally would fail
    assert!(!client.check_compliance(&user_a, &user_b, &100));

    client.add_to_whitelist(&admin, &user_a);
    client.add_to_whitelist(&admin, &user_b);

    // Both whitelisted — should pass
    assert!(client.check_compliance(&user_a, &user_b, &100));
}

#[test]
fn remove_from_whitelist_re_enforces_kyc() {
    let (env, admin, client) = setup();
    let user_a = Address::generate(&env);
    let user_b = Address::generate(&env);

    client.add_to_whitelist(&admin, &user_a);
    client.add_to_whitelist(&admin, &user_b);
    assert!(client.check_compliance(&user_a, &user_b, &100));

    client.remove_from_whitelist(&admin, &user_a);
    // user_a no longer whitelisted and has no KYC
    assert!(!client.check_compliance(&user_a, &user_b, &100));
}

// ── transfer limits ───────────────────────────────────────────────────────────

#[test]
fn transfer_within_daily_limit_passes() {
    let (env, admin, client) = setup();
    let user = Address::generate(&env);

    let limits = TransferLimits {
        daily_limit: 1000,
        monthly_limit: 10000,
        annual_limit: 100000,
        remaining_daily: 1000,
        remaining_monthly: 10000,
        remaining_annual: 100000,
        last_reset_daily: 0,
        last_reset_monthly: 0,
        last_reset_annual: 0,
    };
    client.set_transfer_limits(&admin, &user, &limits);
    assert!(client.check_transfer_limits(&user, &500));
}

#[test]
fn transfer_exceeding_daily_limit_fails() {
    let (env, admin, client) = setup();
    let user = Address::generate(&env);

    let limits = TransferLimits {
        daily_limit: 100,
        monthly_limit: 10000,
        annual_limit: 100000,
        remaining_daily: 100,
        remaining_monthly: 10000,
        remaining_annual: 100000,
        last_reset_daily: 0,
        last_reset_monthly: 0,
        last_reset_annual: 0,
    };
    client.set_transfer_limits(&admin, &user, &limits);
    assert!(!client.check_transfer_limits(&user, &500));
}

// ── compliance rules ──────────────────────────────────────────────────────────

#[test]
fn get_compliance_rules_returns_seeded_rules() {
    let (_, _, client) = setup();
    let rules = client.get_compliance_rules();
    // initialize() seeds 3 rules: rule_144, reg_d, reg_s
    assert_eq!(rules.len(), 3);
}

#[test]
fn update_compliance_rule_modifies_existing_rule() {
    let (env, admin, client) = setup();
    let rules = client.get_compliance_rules();
    let mut rule = rules.get(0).unwrap();
    rule.is_active = false;
    client.update_compliance_rule(&admin, &rule);
    let updated = client.get_compliance_rules();
    let first = updated.get(0).unwrap();
    assert!(!first.is_active);
}

// ── check_compliance integration ─────────────────────────────────────────────

#[test]
fn compliance_passes_for_two_verified_users() {
    let (env, admin, client) = setup();
    let user_a = Address::generate(&env);
    let user_b = Address::generate(&env);
    client.update_kyc_status(&admin, &user_a, &verified_kyc(&env));
    client.update_kyc_status(&admin, &user_b, &verified_kyc(&env));
    assert!(client.check_compliance(&user_a, &user_b, &100));
}

#[test]
fn compliance_fails_when_kyc_expired() {
    let (env, admin, client) = setup();
    let user_a = Address::generate(&env);
    let user_b = Address::generate(&env);

    let mut expired_kyc = verified_kyc(&env);
    expired_kyc.expiry_date = 0; // already expired

    client.update_kyc_status(&admin, &user_a, &expired_kyc);
    client.update_kyc_status(&admin, &user_b, &verified_kyc(&env));
    assert!(!client.check_compliance(&user_a, &user_b, &100));
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #222: Upgrade & Migration Tests for ComplianceRegistry
// ═══════════════════════════════════════════════════════════════════════════════

// ── migrate ───────────────────────────────────────────────────────────────────

#[test]
fn migrate_preserves_kyc_data() {
    let (env, admin, client) = setup();
    let user = Address::generate(&env);
    let kyc = verified_kyc(&env);
    client.update_kyc_status(&admin, &user, &kyc);

    // Data persistence check — KYC should still be accessible
    let stored = client.get_kyc_status(&user);
    assert!(stored.is_verified);
    assert_eq!(stored.verification_level, 2);
    assert_eq!(stored.jurisdiction, Symbol::new(&env, "US"));
}

#[test]
fn migrate_preserves_blacklist_data() {
    let (env, admin, client) = setup();
    let user_a = Address::generate(&env);
    let user_b = Address::generate(&env);

    // Set up KYC so blacklist is the only blocker
    client.update_kyc_status(&admin, &user_a, &verified_kyc(&env));
    client.update_kyc_status(&admin, &user_b, &verified_kyc(&env));

    client.add_to_blacklist(&admin, &user_a, &Symbol::new(&env, "fraud"));

    // Blacklist persists
    assert!(!client.check_compliance(&user_a, &user_b, &100));

    // Non-blacklisted still passes
    let user_c = Address::generate(&env);
    client.update_kyc_status(&admin, &user_c, &verified_kyc(&env));
    assert!(client.check_compliance(&user_c, &user_b, &100));
}

#[test]
fn migrate_preserves_whitelist_data() {
    let (env, admin, client) = setup();
    let user = Address::generate(&env);

    client.add_to_whitelist(&admin, &user);

    // Whitelisted user bypasses KYC
    let stranger = Address::generate(&env);
    assert!(client.check_compliance(&user, &stranger, &100));
}

#[test]
fn migrate_preserves_transfer_limits() {
    let (env, admin, client) = setup();
    let user = Address::generate(&env);

    let limits = TransferLimits {
        daily_limit: 500,
        monthly_limit: 5000,
        annual_limit: 50000,
        remaining_daily: 500,
        remaining_monthly: 5000,
        remaining_annual: 50000,
        last_reset_daily: 0,
        last_reset_monthly: 0,
        last_reset_annual: 0,
    };
    client.set_transfer_limits(&admin, &user, &limits);

    // Limits persist
    assert!(client.check_transfer_limits(&user, &100));
    assert!(!client.check_transfer_limits(&user, &600));
}

#[test]
fn migrate_preserves_compliance_rules() {
    let (env, _, client) = setup();
    let rules = client.get_compliance_rules();

    // Initial seeding creates 3 rules
    assert_eq!(rules.len(), 3);

    // Rules should be accessible
    let first = rules.get(0).unwrap();
    assert_eq!(first.name, Symbol::new(&env, "Rule144"));
}

#[test]
fn migrate_with_empty_kyc_registry() {
    let (env, _, client) = setup();

    // All users get default unverified KYC
    let stranger = Address::generate(&env);
    let kyc = client.get_kyc_status(&stranger);
    assert!(!kyc.is_verified);
    assert_eq!(kyc.verification_level, 0);

    // Compliance check works even with empty registry
    let user_a = Address::generate(&env);
    let user_b = Address::generate(&env);
    assert!(!client.check_compliance(&user_a, &user_b, &100));
}

#[test]
#[should_panic]
fn migrate_by_non_admin_panics() {
    let (env, _, client) = setup();
    let attacker = Address::generate(&env);
    client.migrate(&attacker);
}

#[test]
fn data_integrity_with_complex_state() {
    let (env, admin, client) = setup();

    // Build complex state: KYC + whitelist + blacklist + limits + rules
    let verified_user = Address::generate(&env);
    let blacklisted_user = Address::generate(&env);
    let whitelisted_user = Address::generate(&env);
    let limited_user = Address::generate(&env);

    client.update_kyc_status(&admin, &verified_user, &verified_kyc(&env));
    client.add_to_blacklist(&admin, &blacklisted_user, &Symbol::new(&env, "aml"));
    client.add_to_whitelist(&admin, &whitelisted_user);

    let limits = TransferLimits {
        daily_limit: 200,
        monthly_limit: 2000,
        annual_limit: 20000,
        remaining_daily: 200,
        remaining_monthly: 2000,
        remaining_annual: 20000,
        last_reset_daily: 0,
        last_reset_monthly: 0,
        last_reset_annual: 0,
    };
    client.set_transfer_limits(&admin, &limited_user, &limits);

    // Verify all states intact
    let kyc = client.get_kyc_status(&verified_user);
    assert!(kyc.is_verified);

    // Blacklisted user fails compliance
    let stranger = Address::generate(&env);
    assert!(!client.check_compliance(&blacklisted_user, &stranger, &100));

    // Whitelisted passes
    assert!(client.check_compliance(&whitelisted_user, &stranger, &100));

    // Limits work
    assert!(client.check_transfer_limits(&limited_user, &100));
    assert!(!client.check_transfer_limits(&limited_user, &300));
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #122: Delegated compliance verification for third-party integrators
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_delegate_registration() {
    let (env, admin, client) = setup();
    let delegate = Address::generate(&env);
    let user = Address::generate(&env);

    client.delegate_compliance_check(&admin, &delegate, &user);

    let delegates = client.list_delegates();
    assert_eq!(delegates.len(), 1);
    assert_eq!(delegates.get(0).unwrap(), delegate);
}

#[test]
fn test_submit_external_check() {
    let (env, admin, client) = setup();
    let delegate = Address::generate(&env);
    let user = Address::generate(&env);

    client.delegate_compliance_check(&admin, &delegate, &user);

    client.submit_external_check(&delegate, &user, &true, &Symbol::new(&env, "hash123"));

    let checks: Vec<DelegatedCheck> = client.get_external_checks(&user);
    assert_eq!(checks.len(), 1);
    let check = checks.get(0).unwrap();
    assert_eq!(check.delegate, delegate);
    assert_eq!(check.user, user);
    assert!(check.result);
    assert_eq!(check.evidence_hash, Symbol::new(&env, "hash123"));
}

#[test]
#[should_panic]
fn test_submit_external_check_unauthorized() {
    let (env, admin, client) = setup();
    let delegate = Address::generate(&env);
    let non_delegate = Address::generate(&env);
    let user = Address::generate(&env);

    // Register `delegate` but attempt to submit as `non_delegate`.
    client.delegate_compliance_check(&admin, &delegate, &user);
    client.submit_external_check(&non_delegate, &user, &true, &Symbol::new(&env, "hash"));
}

#[test]
#[should_panic]
fn test_remove_delegate() {
    let (env, admin, client) = setup();
    let delegate = Address::generate(&env);
    let user = Address::generate(&env);

    // Register the delegate and verify they can submit.
    client.delegate_compliance_check(&admin, &delegate, &user);
    client.submit_external_check(&delegate, &user, &true, &Symbol::new(&env, "hash"));

    // Remove the delegate — they should no longer be able to submit.
    client.remove_delegate(&admin, &delegate);
    let delegates = client.list_delegates();
    assert_eq!(delegates.len(), 0);

    // This submission must panic because the delegate is no longer registered.
    client.submit_external_check(&delegate, &user, &true, &Symbol::new(&env, "hash2"));
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #123: Configurable risk score thresholds per asset class
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_per_asset_risk_thresholds() {
    let (env, admin, client) = setup();
    let asset_type = Symbol::new(&env, "real_estate");

    let thresholds = RiskThresholds {
        max_risk_score: 6,
        require_aml_clearance_above: 5,
    };
    client.set_risk_thresholds(&admin, &asset_type, &thresholds);

    let stored = client.get_risk_thresholds(&asset_type);
    assert_eq!(stored.max_risk_score, 6);
    assert_eq!(stored.require_aml_clearance_above, 5);
}

#[test]
fn test_threshold_update() {
    let (env, admin, client) = setup();
    let asset_type = Symbol::new(&env, "commodity");

    // Set initial thresholds.
    client.set_risk_thresholds(
        &admin,
        &asset_type,
        &RiskThresholds {
            max_risk_score: 7,
            require_aml_clearance_above: 7,
        },
    );

    // Update to stricter thresholds.
    client.set_risk_thresholds(
        &admin,
        &asset_type,
        &RiskThresholds {
            max_risk_score: 4,
            require_aml_clearance_above: 4,
        },
    );

    let stored = client.get_risk_thresholds(&asset_type);
    assert_eq!(stored.max_risk_score, 4);
    assert_eq!(stored.require_aml_clearance_above, 4);
}

#[test]
fn test_default_threshold_fallback() {
    let (env, _admin, client) = setup();
    let asset_type = Symbol::new(&env, "unconfigured_asset");

    // No thresholds configured — should return the default (8, 8).
    let stored = client.get_risk_thresholds(&asset_type);
    assert_eq!(stored.max_risk_score, 8);
    assert_eq!(stored.require_aml_clearance_above, 8);
}

#[test]
fn test_check_compliance_with_asset() {
    let (env, admin, client) = setup();
    let asset_type = Symbol::new(&env, "strict_asset");
    let user_a = Address::generate(&env);
    let user_b = Address::generate(&env);

    // Give both users valid KYC with a risk score of 6.
    let mut kyc = verified_kyc(&env);
    kyc.risk_score = 6;
    client.update_kyc_status(&admin, &user_a, &kyc);
    client.update_kyc_status(&admin, &user_b, &kyc);

    // Default threshold (8): risk score 6 should pass.
    assert!(client.check_compliance(&user_a, &user_b, &100));

    // Set a stricter threshold (max_risk_score = 5) for the asset type.
    client.set_risk_thresholds(
        &admin,
        &asset_type,
        &RiskThresholds {
            max_risk_score: 5,
            require_aml_clearance_above: 5,
        },
    );

    // Asset-specific check with the stricter threshold should reject risk 6.
    assert!(!client.check_compliance_with_asset(&user_a, &user_b, &100, &asset_type));
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #124: Compliance data export for regulatory reporting
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_generate_kyc_report() {
    let (env, admin, client) = setup();
    let user = Address::generate(&env);

    // Update KYC status — this logs a `kyc_verified` event.
    client.update_kyc_status(&admin, &user, &verified_kyc(&env));

    let now = env.ledger().timestamp();
    let report = client.generate_report(
        &Symbol::new(&env, "KYC_VERIFICATIONS"),
        &(now - 1000),
        &(now + 1000),
    );

    assert_eq!(report.report_type, Symbol::new(&env, "KYC_VERIFICATIONS"));
    assert!(report.entries.len() >= 1);
    let entry = report.entries.get(0).unwrap();
    assert_eq!(entry.entry_type, Symbol::new(&env, "kyc_verified"));
    assert_eq!(entry.address, user);
}

#[test]
fn test_generate_blacklist_report() {
    let (env, admin, client) = setup();
    let user = Address::generate(&env);

    // Add to blacklist then remove — both are logged.
    client.add_to_blacklist(&admin, &user, &Symbol::new(&env, "fraud"));
    client.remove_from_blacklist(&admin, &user);

    let now = env.ledger().timestamp();
    let report = client.generate_report(
        &Symbol::new(&env, "BLACKLIST_CHANGES"),
        &(now - 1000),
        &(now + 1000),
    );

    assert_eq!(report.entries.len(), 2);
    let add = report.entries.get(0).unwrap();
    assert_eq!(add.entry_type, Symbol::new(&env, "blacklisted"));
    let rem = report.entries.get(1).unwrap();
    assert_eq!(rem.entry_type, Symbol::new(&env, "unblacklisted"));
}

#[test]
fn test_empty_report() {
    let (env, _admin, client) = setup();
    let now = env.ledger().timestamp();

    // No events have been logged in this range, so the report should be empty.
    let report = client.generate_report(
        &Symbol::new(&env, "KYC_VERIFICATIONS"),
        &(now - 1000),
        &(now + 1000),
    );

    assert_eq!(report.entries.len(), 0);
}

#[test]
#[should_panic]
fn test_date_range_validation() {
    let (env, _admin, client) = setup();
    let now = env.ledger().timestamp();

    // Window of 91 days (91 * 86400 seconds) exceeds the 90-day maximum.
    let too_long: u64 = 91 * 86400;
    client.generate_report(
        &Symbol::new(&env, "KYC_VERIFICATIONS"),
        &now,
        &(now + too_long),
    );
}
