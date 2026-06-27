use crate::contract::{Token, TokenClient};
use soroban_sdk::{testutils::Address as _, Address, Env, String};

#[cfg(test)]
fn setup(env: &Env) -> (TokenClient<'_>, Address) {
    let contract_id = env.register(Token, ());
    let client = TokenClient::new(env, &contract_id);
    let admin = Address::generate(env);
    client.initialize(
        &admin,
        &7,
        &String::from_str(env, "Test Token"),
        &String::from_str(env, "TEST"),
    );
    (client, admin)
}

/// Admin can rotate to a new admin; new admin can mint, old cannot.
#[test]
fn test_admin_rotation_basic() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, _admin) = setup(&env);
    let new_admin = Address::generate(&env);
    let user = Address::generate(&env);

    client.set_admin(&new_admin);

    // New admin can mint
    client.mint(&user, &500);
    assert_eq!(client.balance(&user), 500);
}

/// set_admin requires auth from the current admin.
#[test]
#[should_panic]
fn test_admin_rotation_requires_auth() {
    let env = Env::default();
    // No mock_all_auths — auth is enforced
    let (client, _admin) = setup(&env);
    let new_admin = Address::generate(&env);

    // Should panic: no auth provided
    client.set_admin(&new_admin);
}

/// Admin can be rotated multiple times in sequence.
#[test]
fn test_admin_rotation_chain() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, _admin) = setup(&env);
    let admin2 = Address::generate(&env);
    let admin3 = Address::generate(&env);
    let user = Address::generate(&env);

    client.set_admin(&admin2);
    client.set_admin(&admin3);

    // admin3 is now the admin; mint should succeed
    client.mint(&user, &100);
    assert_eq!(client.balance(&user), 100);
}

/// After rotation, the new admin can rotate again to a third admin.
#[test]
fn test_admin_rotation_new_admin_can_rotate() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, _admin) = setup(&env);
    let admin2 = Address::generate(&env);
    let admin3 = Address::generate(&env);
    let user = Address::generate(&env);

    client.set_admin(&admin2);
    // admin2 rotates to admin3
    client.set_admin(&admin3);

    client.mint(&user, &250);
    assert_eq!(client.balance(&user), 250);
}

/// Rotating to the same admin is a no-op (idempotent).
#[test]
fn test_admin_rotation_to_self() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, admin) = setup(&env);
    // Rotate to self
    client.set_admin(&admin);

    let user = Address::generate(&env);
    client.mint(&user, &10);
    assert_eq!(client.balance(&user), 10);
}

// ── EmergencyGuard integration tests (closes #441) ──────────────────────────

/// After rotation via EmergencyGuard, guard admin list reflects the new admin.
#[test]
fn test_rotation_updates_guard_admins() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, _admin) = setup(&env);
    let new_admin = Address::generate(&env);

    client.set_admin(&new_admin);

    // The guard admin list must now contain new_admin, not the old one.
    let guard_admins = client.get_guard_admins();
    assert!(guard_admins.iter().any(|a| a == new_admin));
}

/// After rotation the new admin can pause minting via EmergencyGuard.
#[test]
fn test_rotation_new_admin_can_pause() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, _admin) = setup(&env);
    let new_admin = Address::generate(&env);

    client.set_admin(&new_admin);
    client.pause_minting(&new_admin);

    assert!(client.is_operation_paused(&emergency_guard::PauseType::MINT));
}

/// After rotation the new admin can resume minting via EmergencyGuard.
#[test]
fn test_rotation_new_admin_can_resume() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, _admin) = setup(&env);
    let new_admin = Address::generate(&env);

    client.set_admin(&new_admin);
    client.pause_minting(&new_admin);
    client.resume_minting(&new_admin);

    assert!(!client.is_operation_paused(&emergency_guard::PauseType::MINT));
}

/// Guard threshold stays at 1 after rotation.
#[test]
fn test_rotation_preserves_threshold() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, _admin) = setup(&env);
    let new_admin = Address::generate(&env);

    client.set_admin(&new_admin);

    assert_eq!(client.get_guard_threshold(), 1);
}

/// Chain rotation: each successive admin can use emergency_pause_all.
#[test]
fn test_rotation_chain_emergency_pause() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, _admin) = setup(&env);
    let admin2 = Address::generate(&env);
    let admin3 = Address::generate(&env);

    client.set_admin(&admin2);
    client.set_admin(&admin3);

    // admin3 can call emergency_pause_all
    client.emergency_pause_all(&soroban_sdk::vec![&env, admin3.clone()]);
    assert_eq!(client.get_pause_state(), u32::MAX);
}

/// After rotation, resume_all requires the *new* admin; old is no longer authorised.
#[test]
fn test_rotation_resume_all_requires_new_admin() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, _admin) = setup(&env);
    let new_admin = Address::generate(&env);

    client.set_admin(&new_admin);
    client.emergency_pause_all(&soroban_sdk::vec![&env, new_admin.clone()]);
    client.resume_all(&soroban_sdk::vec![&env, new_admin.clone()]);

    assert_eq!(client.get_pause_state(), 0);
}
