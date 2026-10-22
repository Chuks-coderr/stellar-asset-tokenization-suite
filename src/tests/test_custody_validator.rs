#![cfg(test)]

use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    Address, BytesN, Env, Map, Symbol, Vec,
};

use crate::custody_validator::{
    CustodyAttestation, CustodyValidator, CustodyValidatorClient, InsuranceIntegration,
    PerformanceWeights, MAX_ITERATIONS,
};

// ── helpers ──────────────────────────────────────────────────────────────────

fn setup() -> (Env, Address, Address, CustodyValidatorClient<'static>) {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let oracle = Address::generate(&env);

    let contract_id = env.register_contract(None, CustodyValidator);
    let client = CustodyValidatorClient::new(&env, &contract_id);
    client.initialize(&admin, &admin, &Vec::from_array(&env, [oracle.clone()]));

    // SAFETY: the environment outlives the client for the duration of each test.
    let client: CustodyValidatorClient<'static> = unsafe { core::mem::transmute(client) };
    (env, admin, oracle, client)
}

fn register_custodian(
    env: &Env,
    client: &CustodyValidatorClient<'static>,
    admin: &Address,
    custodian: &Address,
    types: Vec<Symbol>,
) {
    client.register_custodian(
        admin,
        custodian,
        &Symbol::new(env, "Custodian"),
        &Symbol::new(env, "US"),
        &Symbol::new(env, "LIC_001"),
        &types,
        &0i128,
        &Symbol::new(env, "Insurer"),
    );
}

fn make_attestation(
    env: &Env,
    asset_id: &Address,
    custodian: &Address,
    verification_type: &Symbol,
    expires_in: u64,
) -> CustodyAttestation {
    CustodyAttestation {
        asset_id: asset_id.clone(),
        custodian: custodian.clone(),
        location: Symbol::new(env, "Vault"),
        condition: Symbol::new(env, "excellent"),
        value: 1_000_000i128,
        timestamp: env.ledger().timestamp(),
        proof_hash: BytesN::from_array(env, &[1u8; 32]),
        verification_type: verification_type.clone(),
        insurance_status: Symbol::new(env, "insured"),
        legal_title_hash: BytesN::from_array(env, &[2u8; 32]),
        audit_report_hash: BytesN::from_array(env, &[3u8; 32]),
        multi_sig_signatures: Vec::from_array(env, [BytesN::from_array(env, &[4u8; 64])]),
        metadata: Map::new(env),
        is_valid: false,
        expires_at: env.ledger().timestamp() + expires_in,
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #162: custodian verification-type authorization
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn authorized_verification_type_succeeds() {
    let (env, admin, _oracle, client) = setup();
    let custodian = Address::generate(&env);
    register_custodian(
        &env,
        &client,
        &admin,
        &custodian,
        Vec::from_array(&env, [Symbol::new(&env, "real_estate")]),
    );

    let asset = Address::generate(&env);
    let attestation = make_attestation(
        &env,
        &asset,
        &custodian,
        &Symbol::new(&env, "real_estate"),
        86400 * 30,
    );

    let id = client.submit_attestation(&attestation);
    assert_eq!(id, 1);
    assert!(client.is_attestation_valid(&id));
}

#[test]
#[should_panic]
fn unauthorized_verification_type_rejected() {
    let (env, admin, _oracle, client) = setup();
    let custodian = Address::generate(&env);
    register_custodian(
        &env,
        &client,
        &admin,
        &custodian,
        Vec::from_array(&env, [Symbol::new(&env, "real_estate")]),
    );

    let asset = Address::generate(&env);
    let attestation = make_attestation(
        &env,
        &asset,
        &custodian,
        &Symbol::new(&env, "precious_metals"),
        86400 * 30,
    );

    client.submit_attestation(&attestation);
}

#[test]
fn update_verification_types_enables_new_type() {
    let (env, admin, _oracle, client) = setup();
    let custodian = Address::generate(&env);
    register_custodian(
        &env,
        &client,
        &admin,
        &custodian,
        Vec::from_array(&env, [Symbol::new(&env, "real_estate")]),
    );

    client.set_custodian_verification_types(
        &admin,
        &custodian,
        &Vec::from_array(&env, [Symbol::new(&env, "commodities")]),
    );

    let updated = client.get_custodian_info(&custodian);
    assert!(updated
        .verification_types
        .contains(&Symbol::new(&env, "commodities")));

    let asset = Address::generate(&env);
    let attestation = make_attestation(
        &env,
        &asset,
        &custodian,
        &Symbol::new(&env, "commodities"),
        86400 * 30,
    );
    let id = client.submit_attestation(&attestation);
    assert_eq!(id, 1);
}

#[test]
#[should_panic]
fn update_verification_types_by_non_admin_panics() {
    let (env, admin, _oracle, client) = setup();
    let custodian = Address::generate(&env);
    register_custodian(
        &env,
        &client,
        &admin,
        &custodian,
        Vec::from_array(&env, [Symbol::new(&env, "real_estate")]),
    );

    let attacker = Address::generate(&env);
    client.set_custodian_verification_types(
        &attacker,
        &custodian,
        &Vec::from_array(&env, [Symbol::new(&env, "commodities")]),
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #166: immutable custody audit trail
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn audit_records_registrations() {
    let (env, admin, oracle, client) = setup();
    let custodian = Address::generate(&env);
    register_custodian(
        &env,
        &client,
        &admin,
        &custodian,
        Vec::from_array(&env, [Symbol::new(&env, "real_estate")]),
    );

    let custodian_trail = client.get_audit_trail(&custodian, &0u64, &0u64);
    assert_eq!(custodian_trail.len(), 1);
    let entry = custodian_trail.get(0).unwrap();
    assert!(entry.event_type == Symbol::new(&env, "registered"));
    assert!(entry.actor == admin);

    client.register_oracle(
        &admin,
        &oracle,
        &Symbol::new(&env, "Oracle"),
        &Symbol::new(&env, "US"),
    );
    let oracle_trail = client.get_audit_trail(&oracle, &0u64, &0u64);
    assert_eq!(oracle_trail.len(), 1);
}

#[test]
fn audit_records_attestation_and_invalidation_with_time_filter() {
    let (env, admin, _oracle, client) = setup();
    let custodian = Address::generate(&env);
    register_custodian(
        &env,
        &client,
        &admin,
        &custodian,
        Vec::from_array(&env, [Symbol::new(&env, "real_estate")]),
    );

    let asset = Address::generate(&env);
    let attestation = make_attestation(
        &env,
        &asset,
        &custodian,
        &Symbol::new(&env, "real_estate"),
        86400 * 30,
    );
    let id = client.submit_attestation(&attestation);

    let at_submission = client.get_audit_trail(&asset, &0u64, &0u64);
    assert_eq!(at_submission.len(), 1);
    assert!(
        at_submission.get(0).unwrap().event_type == Symbol::new(&env, "attestation_submitted")
    );

    env.ledger().set_timestamp(1_000);
    client.invalidate_attestation(&admin, &id);

    let all = client.get_audit_trail(&asset, &0u64, &0u64);
    assert_eq!(all.len(), 2);
    assert!(all.get(1).unwrap().event_type == Symbol::new(&env, "invalidated"));
    assert_eq!(all.get(1).unwrap().timestamp, 1_000);

    let later = client.get_audit_trail(&asset, &500u64, &0u64);
    assert_eq!(later.len(), 1);
    assert!(later.get(0).unwrap().event_type == Symbol::new(&env, "invalidated"));
}

#[test]
fn audit_records_dispute_and_resolution() {
    let (env, admin, _oracle, client) = setup();
    let custodian = Address::generate(&env);
    register_custodian(
        &env,
        &client,
        &admin,
        &custodian,
        Vec::from_array(&env, [Symbol::new(&env, "real_estate")]),
    );

    let asset = Address::generate(&env);
    let attestation = make_attestation(
        &env,
        &asset,
        &custodian,
        &Symbol::new(&env, "real_estate"),
        86400 * 30,
    );
    let id = client.submit_attestation(&attestation);

    let challenger = Address::generate(&env);
    let dispute_id = client.dispute_attestation(
        &id,
        &challenger,
        &Symbol::new(&env, "fraud"),
        &0i128,
        &BytesN::from_array(&env, &[7u8; 32]),
    );
    client.resolve_dispute(&admin, &dispute_id, &Symbol::new(&env, "upheld"), &0i128);

    let trail = client.get_audit_trail(&asset, &0u64, &0u64);
    assert_eq!(trail.len(), 3);
    assert!(trail.get(1).unwrap().event_type == Symbol::new(&env, "dispute_filed"));
    assert!(trail.get(2).unwrap().event_type == Symbol::new(&env, "resolved"));
}

#[test]
fn audit_records_insurance_claim() {
    let (env, admin, _oracle, client) = setup();
    let asset = Address::generate(&env);

    let insurance = InsuranceIntegration {
        provider: Symbol::new(&env, "Insurer"),
        policy_number: Symbol::new(&env, "POL_1"),
        coverage_amount: 1_000_000i128,
        premium_amount: 1_000i128,
        valid_until: env.ledger().timestamp() + 86400 * 365,
        claim_auto_trigger: true,
        last_premium_paid: env.ledger().timestamp(),
        is_active: true,
    };
    client.setup_insurance_integration(&admin, &asset, &insurance);
    client.trigger_insurance_claim(
        &admin,
        &asset,
        &Symbol::new(&env, "damage"),
        &BytesN::from_array(&env, &[9u8; 32]),
    );

    let trail = client.get_audit_trail(&asset, &0u64, &0u64);
    assert_eq!(trail.len(), 1);
    assert!(trail.get(0).unwrap().event_type == Symbol::new(&env, "insurance_claimed"));
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #167: weighted custodian performance scoring
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn performance_score_after_attestation() {
    let (env, admin, _oracle, client) = setup();
    let custodian = Address::generate(&env);
    register_custodian(
        &env,
        &client,
        &admin,
        &custodian,
        Vec::from_array(&env, [Symbol::new(&env, "real_estate")]),
    );

    let asset = Address::generate(&env);
    let attestation = make_attestation(
        &env,
        &asset,
        &custodian,
        &Symbol::new(&env, "real_estate"),
        86400 * 30,
    );
    client.submit_attestation(&attestation);

    let score = client.get_performance_score(&custodian);
    assert_eq!(score.accuracy, 100);
    assert_eq!(score.timeliness, 100);
    assert_eq!(score.thoroughness, 100);
    assert_eq!(score.total_score, 100);
    assert_eq!(score.accuracy_weight, 50);
    assert_eq!(score.timeliness_weight, 25);
    assert_eq!(score.thoroughness_weight, 25);
}

#[test]
fn dispute_lowers_accuracy() {
    let (env, admin, _oracle, client) = setup();
    let custodian = Address::generate(&env);
    register_custodian(
        &env,
        &client,
        &admin,
        &custodian,
        Vec::from_array(&env, [Symbol::new(&env, "real_estate")]),
    );

    let asset = Address::generate(&env);
    let attestation = make_attestation(
        &env,
        &asset,
        &custodian,
        &Symbol::new(&env, "real_estate"),
        86400 * 30,
    );
    let id = client.submit_attestation(&attestation);

    let before = client.get_performance_score(&custodian);
    assert_eq!(before.accuracy, 100);

    let challenger = Address::generate(&env);
    let dispute_id = client.dispute_attestation(
        &id,
        &challenger,
        &Symbol::new(&env, "fraud"),
        &0i128,
        &BytesN::from_array(&env, &[7u8; 32]),
    );
    client.resolve_dispute(&admin, &dispute_id, &Symbol::new(&env, "upheld"), &0i128);

    let after = client.get_performance_score(&custodian);
    assert_eq!(after.accuracy, 0);
    assert!(after.total_score < before.total_score);
}

#[test]
fn activity_improves_accuracy_after_dispute() {
    let (env, admin, _oracle, client) = setup();
    let custodian = Address::generate(&env);
    let verification_type = Symbol::new(&env, "real_estate");
    register_custodian(
        &env,
        &client,
        &admin,
        &custodian,
        Vec::from_array(&env, [verification_type.clone()]),
    );

    let asset = Address::generate(&env);
    let attestation = make_attestation(&env, &asset, &custodian, &verification_type, 86400 * 30);
    let id = client.submit_attestation(&attestation);

    let challenger = Address::generate(&env);
    let dispute_id = client.dispute_attestation(
        &id,
        &challenger,
        &Symbol::new(&env, "fraud"),
        &0i128,
        &BytesN::from_array(&env, &[7u8; 32]),
    );
    client.resolve_dispute(&admin, &dispute_id, &Symbol::new(&env, "upheld"), &0i128);

    let low = client.get_performance_score(&custodian);
    assert_eq!(low.accuracy, 0);

    for _ in 0..4 {
        let new_asset = Address::generate(&env);
        let attestation =
            make_attestation(&env, &new_asset, &custodian, &verification_type, 86400 * 30);
        client.submit_attestation(&attestation);
    }

    let improved = client.get_performance_score(&custodian);
    assert!(improved.accuracy > low.accuracy);
    assert_eq!(improved.accuracy, 80);
}

#[test]
fn update_weights_recomputes_total() {
    let (env, admin, _oracle, client) = setup();
    let custodian = Address::generate(&env);
    register_custodian(
        &env,
        &client,
        &admin,
        &custodian,
        Vec::from_array(&env, [Symbol::new(&env, "real_estate")]),
    );

    let asset = Address::generate(&env);
    let attestation = make_attestation(
        &env,
        &asset,
        &custodian,
        &Symbol::new(&env, "real_estate"),
        86400 * 30,
    );
    client.submit_attestation(&attestation);

    client.update_performance_weights(
        &admin,
        &PerformanceWeights {
            accuracy: 0,
            timeliness: 0,
            thoroughness: 100,
        },
    );

    let score = client.get_performance_score(&custodian);
    assert_eq!(score.thoroughness_weight, 100);
    assert_eq!(score.accuracy_weight, 0);
    assert_eq!(score.total_score, score.thoroughness);
}

#[test]
#[should_panic]
fn update_weights_by_non_admin_panics() {
    let (env, _admin, _oracle, client) = setup();
    let attacker = Address::generate(&env);
    client.update_performance_weights(
        &attacker,
        &PerformanceWeights {
            accuracy: 50,
            timeliness: 25,
            thoroughness: 25,
        },
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Issue #168: bounded iteration and pagination
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn pagination_within_bounds() {
    let (env, admin, _oracle, client) = setup();
    let custodian = Address::generate(&env);
    let verification_type = Symbol::new(&env, "real_estate");
    register_custodian(
        &env,
        &client,
        &admin,
        &custodian,
        Vec::from_array(&env, [verification_type.clone()]),
    );

    for _ in 0..3 {
        let asset = Address::generate(&env);
        let attestation = make_attestation(&env, &asset, &custodian, &verification_type, 86400 * 30);
        client.submit_attestation(&attestation);
    }

    let (page, next) = client.get_attestations(&0u64, &10u32);
    assert_eq!(page.len(), 3);
    assert!(next.is_none());
}

#[test]
fn pagination_continuation_cursor() {
    let (env, admin, _oracle, client) = setup();
    let custodian = Address::generate(&env);
    let verification_type = Symbol::new(&env, "real_estate");
    register_custodian(
        &env,
        &client,
        &admin,
        &custodian,
        Vec::from_array(&env, [verification_type.clone()]),
    );

    for _ in 0..5 {
        let asset = Address::generate(&env);
        let attestation = make_attestation(&env, &asset, &custodian, &verification_type, 86400 * 30);
        client.submit_attestation(&attestation);
    }

    let (first, cursor1) = client.get_attestations(&0u64, &2u32);
    assert_eq!(first.len(), 2);
    assert!(cursor1 == Some(2u64));

    let (second, cursor2) = client.get_attestations(&cursor1.unwrap(), &2u32);
    assert_eq!(second.len(), 2);
    assert!(cursor2 == Some(4u64));

    let (third, cursor3) = client.get_attestations(&cursor2.unwrap(), &2u32);
    assert_eq!(third.len(), 1);
    assert!(cursor3.is_none());
}

#[test]
fn pagination_limit_capped_at_max_iterations() {
    let (env, admin, _oracle, client) = setup();
    let custodian = Address::generate(&env);
    let verification_type = Symbol::new(&env, "real_estate");
    register_custodian(
        &env,
        &client,
        &admin,
        &custodian,
        Vec::from_array(&env, [verification_type.clone()]),
    );

    for _ in 0..3 {
        let asset = Address::generate(&env);
        let attestation = make_attestation(&env, &asset, &custodian, &verification_type, 86400 * 30);
        client.submit_attestation(&attestation);
    }

    let (page, next) = client.get_attestations(&0u64, &1_000u32);
    assert_eq!(page.len(), 3);
    assert!(next.is_none());
}

#[test]
fn pagination_at_boundary_returns_continuation_cursor() {
    let (env, admin, _oracle, client) = setup();
    let custodian = Address::generate(&env);
    let verification_type = Symbol::new(&env, "real_estate");
    register_custodian(
        &env,
        &client,
        &admin,
        &custodian,
        Vec::from_array(&env, [verification_type.clone()]),
    );

    for _ in 0..(MAX_ITERATIONS + 5) {
        let asset = Address::generate(&env);
        let attestation = make_attestation(&env, &asset, &custodian, &verification_type, 86400 * 30);
        client.submit_attestation(&attestation);
    }

    let (page, next) = client.get_attestations(&0u64, &1_000u32);
    assert_eq!(page.len(), MAX_ITERATIONS);
    assert!(next == Some(MAX_ITERATIONS as u64));

    let (tail, tail_next) = client.get_attestations(&(MAX_ITERATIONS as u64), &5u32);
    assert_eq!(tail.len(), 5);
    assert!(tail_next.is_none());
}

#[test]
fn pagination_cursor_past_end_returns_empty() {
    let (env, admin, _oracle, client) = setup();
    let custodian = Address::generate(&env);
    let verification_type = Symbol::new(&env, "real_estate");
    register_custodian(
        &env,
        &client,
        &admin,
        &custodian,
        Vec::from_array(&env, [verification_type.clone()]),
    );

    let asset = Address::generate(&env);
    let attestation = make_attestation(&env, &asset, &custodian, &verification_type, 86400 * 30);
    client.submit_attestation(&attestation);

    let (page, next) = client.get_attestations(&999u64, &10u32);
    assert_eq!(page.len(), 0);
    assert!(next.is_none());
}
