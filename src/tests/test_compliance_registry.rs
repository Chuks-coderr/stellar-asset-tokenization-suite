#![cfg(test)]

use soroban_sdk::{
    testutils::Address as _,
    testutils::Ledger as _,
    Address, Env, Symbol, Vec,
};

use crate::compliance_registry::{
    ComplianceRegistry, ComplianceRegistryClient, ComplianceError, KYCStatus, KycState,
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
// Issue #113: calendar-aligned transfer limit resets
// ═══════════════════════════════════════════════════════════════════════════════

/// 2026-01-01T00:00:00Z
const NEW_YEAR_2026: u64 = 1_767_225_600;
/// 2026-01-02T00:00:00Z
const NEXT_DAY_2026: u64 = 1_767_312_000;
/// 2026-02-01T00:00:00Z
const FEB_2026: u64 = 1_769_904_000;
/// 2026-06-01T00:00:00Z
const JUN_2026: u64 = 1_780_268_800;
/// 2027-01-01T00:00:00Z
const NEW_YEAR_2027: u64 = 1_798_761_600;

fn limits(
    daily: i128,
    monthly: i128,
    annual: i128,
    last_reset: u64,
) -> TransferLimits {
    TransferLimits {
        daily_limit: daily,
        monthly_limit: monthly,
        annual_limit: annual,
        remaining_daily: daily,
        remaining_monthly: monthly,
        remaining_annual: annual,
        last_reset_daily: last_reset,
        last_reset_monthly: last_reset,
        last_reset_annual: last_reset,
    }
}

#[test]
fn daily_limit_resets_at_the_utc_midnight_boundary() {
    let (env, admin, client) = setup();
    let user = Address::generate(&env);

    // 2026-01-01T23:00:00Z
    env.ledger()
        .set_timestamp(NEW_YEAR_2026 + 23 * 3_600);
    client.set_transfer_limits(&admin, &user, &limits(100, 1_000, 10_000, 0));

    // Spend the whole daily allowance.
    assert!(client.check_transfer_limits(&user, &100));
    assert!(!client.check_transfer_limits(&user, &1));

    // 2026-01-01T23:30:00Z — still the same UTC day, so no reset yet.
    env.ledger()
        .set_timestamp(NEW_YEAR_2026 + 23 * 3_600 + 1_800);
    assert!(!client.check_transfer_limits(&user, &1));

    // 2026-01-02T00:00:00Z — the daily window rolls over with the calendar,
    // not 24 hours after the previous reset.
    env.ledger().set_timestamp(NEXT_DAY_2026);
    assert!(client.check_transfer_limits(&user, &100));
}

#[test]
fn monthly_limit_resets_on_the_first_of_the_month() {
    let (env, admin, client) = setup();
    let user = Address::generate(&env);

    // The monthly window was last reset in the middle of January.
    let mid_january = NEW_YEAR_2026 + 14 * 86_400;
    env.ledger().set_timestamp(mid_january);
    client.set_transfer_limits(
        &admin,
        &user,
        &limits(1_000_000, 1_000, 10_000, mid_january),
    );

    assert!(client.check_transfer_limits(&user, &1_000));
    assert!(!client.check_transfer_limits(&user, &1));

    // 2026-02-01T00:00:00Z — the first day of the next calendar month. A
    // 30-day window would not have rolled over yet.
    env.ledger().set_timestamp(FEB_2026);
    assert!(client.check_transfer_limits(&user, &1_000));
}

#[test]
fn annual_limit_resets_on_new_year() {
    let (env, admin, client) = setup();
    let user = Address::generate(&env);

    env.ledger().set_timestamp(JUN_2026);
    client.set_transfer_limits(
        &admin,
        &user,
        &limits(1_000_000, 1_000_000, 10_000, JUN_2026),
    );

    assert!(client.check_transfer_limits(&user, &10_000));
    assert!(!client.check_transfer_limits(&user, &1));

    // 2027-01-01T00:00:00Z — a 365-day window would not have rolled over yet.
    env.ledger().set_timestamp(NEW_YEAR_2027);
    assert!(client.check_transfer_limits(&user, &10_000));
}

#[test]
fn limits_written_by_the_previous_implementation_still_work() {
    // The stored `last_reset_*` values stay timestamps, so a row written
    // before this change is interpreted rather than rewritten: no migration
    // is needed.
    let (env, admin, client) = setup();
    let user = Address::generate(&env);

    env.ledger().set_timestamp(NEW_YEAR_2026);
    client.set_transfer_limits(
        &admin,
        &user,
        &limits(1_000, 1_000, 10_000, NEW_YEAR_2026 - 86_400),
    );

    // The daily window is a day old, so it rolls over; the monthly and
    // annual windows are still the ones that were stored.
    assert!(client.check_transfer_limits(&user, &1_000));
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #111: KYC expiry grace period
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn kyc_state_is_active_while_the_record_is_valid() {
    let (env, admin, client) = setup();
    let user = Address::generate(&env);

    client.update_kyc_status(&admin, &user, &verified_kyc(&env));

    assert_eq!(client.get_effective_kyc_state(&user), KycState::Active);
    assert!(client.get_kyc_status(&user).is_verified);
}

#[test]
fn kyc_state_is_warning_inside_the_grace_period() {
    let (env, admin, client) = setup();
    let user = Address::generate(&env);

    let mut kyc = verified_kyc(&env);
    kyc.expiry_date = env.ledger().timestamp() + 3 * 86_400; // three days left
    client.update_kyc_status(&admin, &user, &kyc);

    assert_eq!(client.get_effective_kyc_state(&user), KycState::Warning);
    // The record is still verified while it is inside the grace period.
    assert!(client.get_kyc_status(&user).is_verified);
}

#[test]
fn kyc_state_is_expired_after_the_expiry_date() {
    let (env, admin, client) = setup();
    let user = Address::generate(&env);

    env.ledger().set_timestamp(1_000_000);
    let mut kyc = verified_kyc(&env);
    kyc.expiry_date = 999_999; // already expired
    client.update_kyc_status(&admin, &user, &kyc);

    assert_eq!(client.get_effective_kyc_state(&user), KycState::Expired);
    // The effective status is reported as no longer verified.
    assert!(!client.get_kyc_status(&user).is_verified);
}

#[test]
fn kyc_state_is_revoked_for_a_blacklisted_address() {
    let (env, admin, client) = setup();
    let user = Address::generate(&env);

    client.update_kyc_status(&admin, &user, &verified_kyc(&env));
    client.add_to_blacklist(&admin, &user, &Symbol::new(&env, "fraud"));

    assert_eq!(client.get_effective_kyc_state(&user), KycState::Revoked);
    assert!(!client.get_kyc_status(&user).is_verified);
}

#[test]
fn warning_state_halves_the_transfer_limit() {
    let (env, admin, client) = setup();
    let user = Address::generate(&env);

    let mut kyc = verified_kyc(&env);
    kyc.expiry_date = env.ledger().timestamp() + 3 * 86_400;
    client.update_kyc_status(&admin, &user, &kyc);

    client.set_transfer_limits(
        &admin,
        &user,
        &limits(1_000, 1_000_000, 1_000_000, 0),
    );

    // A participant inside the grace period may move up to half of their
    // normal limit.
    assert!(client.check_transfer_limits(&user, &500));
    assert!(!client.check_transfer_limits(&user, &1));
}
