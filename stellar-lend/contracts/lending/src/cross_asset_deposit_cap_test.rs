//! Per-Asset Deposit Cap Tests
//!
//! Comprehensive test coverage for per-asset deposit caps including:
//! - Basic cap enforcement during deposits
//! - Boundary conditions (at cap, just below, just above)
//! - Cap updates and their effects
//! - Interaction with paused states
//! - Multiple users and multiple assets
//! - Withdrawal effects on cap tracking
//! - View function correctness

use crate::cross_asset::{AssetParams, CrossAssetError};
use crate::{LendingContract, LendingContractClient};
use soroban_sdk::{testutils::Address as _, Address, Env};

// ============================================================================
// Test Helpers
// ============================================================================

fn setup() -> (Env, LendingContractClient<'static>, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(LendingContract, ());
    let client = LendingContractClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let user = Address::generate(&env);
    
    // Initialize contract
    client.initialize(&admin, 1_000_000_000_000, 1000);
    client.initialize_ca(&admin);
    
    (env, client, admin, user)
}

fn create_asset_params(env: &Env, deposit_cap: i128) -> AssetParams {
    AssetParams {
        ltv: 7500,                    // 75% LTV
        liquidation_threshold: 8000,  // 80%
        price_feed: Address::generate(env),
        debt_ceiling: 10_000_000_000_000,
        deposit_cap,
        is_active: true,
    }
}

fn register_asset_with_cap(
    client: &LendingContractClient,
    admin: &Address,
    asset: &Address,
    deposit_cap: i128,
) {
    client.register_asset(admin, asset);
    let params = create_asset_params(&client.env(), deposit_cap);
    client.set_asset_params(asset, &params);
}

// ============================================================================
// Basic Deposit Cap Enforcement Tests
// ============================================================================

#[test]
fn test_deposit_within_cap_succeeds() {
    let (env, client, admin, user) = setup();
    let asset = Address::generate(&env);
    let cap = 1_000_000;
    
    register_asset_with_cap(&client, &admin, &asset, cap);
    
    // Deposit well within cap
    let result = client.deposit_collateral_asset(&user, &asset, &500_000);
    assert!(result.is_ok());
    
    // Verify total deposits
    let total = client.get_asset_total_deposits(&asset);
    assert_eq!(total, 500_000);
}

#[test]
fn test_deposit_at_exact_cap_succeeds() {
    let (env, client, admin, user) = setup();
    let asset = Address::generate(&env);
    let cap = 1_000_000;
    
    register_asset_with_cap(&client, &admin, &asset, cap);
    
    // Deposit exactly at cap
    let result = client.deposit_collateral_asset(&user, &asset, &cap);
    assert!(result.is_ok());
    
    let total = client.get_asset_total_deposits(&asset);
    assert_eq!(total, cap);
}

#[test]
fn test_deposit_exceeds_cap_fails() {
    let (env, client, admin, user) = setup();
    let asset = Address::generate(&env);
    let cap = 1_000_000;
    
    register_asset_with_cap(&client, &admin, &asset, cap);
    
    // Try to deposit above cap
    let result = client.try_deposit_collateral_asset(&user, &asset, &(cap + 1));
    assert_eq!(result, Err(Ok(CrossAssetError::ExceedsDepositCap)));
}

#[test]
fn test_incremental_deposits_respect_cap() {
    let (env, client, admin, user) = setup();
    let asset = Address::generate(&env);
    let cap = 1_000_000;
    
    register_asset_with_cap(&client, &admin, &asset, cap);
    
    // First deposit
    client.deposit_collateral_asset(&user, &asset, &600_000);
    
    // Second deposit within remaining capacity
    let result = client.deposit_collateral_asset(&user, &asset, &300_000);
    assert!(result.is_ok());
    
    // Third deposit exceeds cap
    let result = client.try_deposit_collateral_asset(&user, &asset, &200_000);
    assert_eq!(result, Err(Ok(CrossAssetError::ExceedsDepositCap)));
}

// ============================================================================
// Boundary Condition Tests
// ============================================================================

#[test]
fn test_deposit_one_below_cap_then_one_more_fails() {
    let (env, client, admin, user) = setup();
    let asset = Address::generate(&env);
    let cap = 1_000_000;
    
    register_asset_with_cap(&client, &admin, &asset, cap);
    
    // Deposit one below cap
    client.deposit_collateral_asset(&user, &asset, &(cap - 1));
    
    // Try to deposit one more
    let result = client.try_deposit_collateral_asset(&user, &asset, &2);
    assert_eq!(result, Err(Ok(CrossAssetError::ExceedsDepositCap)));
}

#[test]
fn test_deposit_one_below_cap_then_exactly_one_succeeds() {
    let (env, client, admin, user) = setup();
    let asset = Address::generate(&env);
    let cap = 1_000_000;
    
    register_asset_with_cap(&client, &admin, &asset, cap);
    
    // Deposit one below cap
    client.deposit_collateral_asset(&user, &asset, &(cap - 1));
    
    // Deposit exactly one more to reach cap
    let result = client.deposit_collateral_asset(&user, &asset, &1);
    assert!(result.is_ok());
    
    let total = client.get_asset_total_deposits(&asset);
    assert_eq!(total, cap);
}

#[test]
fn test_zero_cap_allows_unlimited_deposits() {
    let (env, client, admin, user) = setup();
    let asset = Address::generate(&env);
    
    // Zero cap means unlimited
    register_asset_with_cap(&client, &admin, &asset, 0);
    
    // Large deposit should succeed
    let huge_amount = 999_999_999_999_999;
    let result = client.deposit_collateral_asset(&user, &asset, &huge_amount);
    assert!(result.is_ok());
}

// ============================================================================
// Cap Update Tests
// ============================================================================

#[test]
fn test_increasing_cap_allows_more_deposits() {
    let (env, client, admin, user) = setup();
    let asset = Address::generate(&env);
    let initial_cap = 1_000_000;
    
    register_asset_with_cap(&client, &admin, &asset, initial_cap);
    
    // Fill to cap
    client.deposit_collateral_asset(&user, &asset, &initial_cap);
    
    // Increase cap
    let new_cap = 2_000_000;
    let params = create_asset_params(&env, new_cap);
    client.set_asset_params(&asset, &params);
    
    // Now can deposit more
    let result = client.deposit_collateral_asset(&user, &asset, &500_000);
    assert!(result.is_ok());
}

#[test]
fn test_decreasing_cap_below_current_deposits_blocks_new_deposits() {
    let (env, client, admin, user) = setup();
    let asset = Address::generate(&env);
    let initial_cap = 2_000_000;
    
    register_asset_with_cap(&client, &admin, &asset, initial_cap);
    
    // Deposit 1.5M
    client.deposit_collateral_asset(&user, &asset, &1_500_000);
    
    // Decrease cap to 1M (below current deposits)
    let new_cap = 1_000_000;
    let params = create_asset_params(&env, new_cap);
    client.set_asset_params(&asset, &params);
    
    // Cannot deposit more even though we're already above cap
    let result = client.try_deposit_collateral_asset(&user, &asset, &1);
    assert_eq!(result, Err(Ok(CrossAssetError::ExceedsDepositCap)));
}

// ============================================================================
// Pause Interaction Tests
// ============================================================================

#[test]
fn test_deposit_cap_enforced_even_when_not_paused() {
    let (env, client, admin, user) = setup();
    let asset = Address::generate(&env);
    let cap = 1_000_000;
    
    register_asset_with_cap(&client, &admin, &asset, cap);
    
    // Ensure not paused
    client.set_pause(&admin, &crate::pause::PauseType::Deposit, &false);
    
    // Cap still enforced
    let result = client.try_deposit_collateral_asset(&user, &asset, &(cap + 1));
    assert_eq!(result, Err(Ok(CrossAssetError::ExceedsDepositCap)));
}

#[test]
fn test_deposit_paused_takes_precedence_over_cap() {
    let (env, client, admin, user) = setup();
    let asset = Address::generate(&env);
    let cap = 1_000_000;
    
    register_asset_with_cap(&client, &admin, &asset, cap);
    
    // Pause deposits
    client.set_pause(&admin, &crate::pause::PauseType::Deposit, &true);
    
    // Deposit within cap fails due to pause
    let result = client.try_deposit_collateral_asset(&user, &asset, &500_000);
    assert_eq!(result, Err(Ok(CrossAssetError::ProtocolPaused)));
}

// ============================================================================
// Multi-User Tests
// ============================================================================

#[test]
fn test_multiple_users_share_same_cap() {
    let (env, client, admin, user1) = setup();
    let user2 = Address::generate(&env);
    let user3 = Address::generate(&env);
    let asset = Address::generate(&env);
    let cap = 1_000_000;
    
    register_asset_with_cap(&client, &admin, &asset, cap);
    
    // User 1 deposits 400k
    client.deposit_collateral_asset(&user1, &asset, &400_000);
    
    // User 2 deposits 400k
    client.deposit_collateral_asset(&user2, &asset, &400_000);
    
    // User 3 can only deposit 200k (remaining capacity)
    let result = client.deposit_collateral_asset(&user3, &asset, &200_000);
    assert!(result.is_ok());
    
    // User 3 cannot deposit more
    let result = client.try_deposit_collateral_asset(&user3, &asset, &1);
    assert_eq!(result, Err(Ok(CrossAssetError::ExceedsDepositCap)));
}

// ============================================================================
// Multi-Asset Tests
// ============================================================================

#[test]
fn test_different_assets_have_independent_caps() {
    let (env, client, admin, user) = setup();
    let asset1 = Address::generate(&env);
    let asset2 = Address::generate(&env);
    
    register_asset_with_cap(&client, &admin, &asset1, 1_000_000);
    register_asset_with_cap(&client, &admin, &asset2, 2_000_000);
    
    // Fill asset1 to cap
    client.deposit_collateral_asset(&user, &asset1, &1_000_000);
    
    // Asset2 still has full capacity
    let result = client.deposit_collateral_asset(&user, &asset2, &2_000_000);
    assert!(result.is_ok());
    
    // Asset1 is still at cap
    let result = client.try_deposit_collateral_asset(&user, &asset1, &1);
    assert_eq!(result, Err(Ok(CrossAssetError::ExceedsDepositCap)));
}

// ============================================================================
// Withdrawal Effects Tests
// ============================================================================

#[test]
fn test_withdrawal_frees_up_cap_space() {
    let (env, client, admin, user) = setup();
    let asset = Address::generate(&env);
    let cap = 1_000_000;
    
    register_asset_with_cap(&client, &admin, &asset, cap);
    
    // Fill to cap
    client.deposit_collateral_asset(&user, &asset, &cap);
    
    // Cannot deposit more
    let result = client.try_deposit_collateral_asset(&user, &asset, &1);
    assert_eq!(result, Err(Ok(CrossAssetError::ExceedsDepositCap)));
    
    // Withdraw some
    client.withdraw_asset(&user, &asset, &300_000);
    
    // Now can deposit again
    let result = client.deposit_collateral_asset(&user, &asset, &300_000);
    assert!(result.is_ok());
}

#[test]
fn test_withdrawal_updates_total_deposits_correctly() {
    let (env, client, admin, user) = setup();
    let asset = Address::generate(&env);
    let cap = 1_000_000;
    
    register_asset_with_cap(&client, &admin, &asset, cap);
    
    // Deposit 800k
    client.deposit_collateral_asset(&user, &asset, &800_000);
    assert_eq!(client.get_asset_total_deposits(&asset), 800_000);
    
    // Withdraw 300k
    client.withdraw_asset(&user, &asset, &300_000);
    assert_eq!(client.get_asset_total_deposits(&asset), 500_000);
    
    // Deposit 400k (should succeed as total would be 900k)
    let result = client.deposit_collateral_asset(&user, &asset, &400_000);
    assert!(result.is_ok());
    assert_eq!(client.get_asset_total_deposits(&asset), 900_000);
}

// ============================================================================
// View Function Tests
// ============================================================================

#[test]
fn test_get_asset_deposit_cap_returns_correct_value() {
    let (env, client, admin, _user) = setup();
    let asset = Address::generate(&env);
    let cap = 5_000_000;
    
    register_asset_with_cap(&client, &admin, &asset, cap);
    
    let retrieved_cap = client.get_asset_deposit_cap(&asset).unwrap();
    assert_eq!(retrieved_cap, cap);
}

#[test]
fn test_get_asset_total_deposits_tracks_correctly() {
    let (env, client, admin, user) = setup();
    let asset = Address::generate(&env);
    
    register_asset_with_cap(&client, &admin, &asset, 10_000_000);
    
    // Initially zero
    assert_eq!(client.get_asset_total_deposits(&asset), 0);
    
    // After first deposit
    client.deposit_collateral_asset(&user, &asset, &1_000_000);
    assert_eq!(client.get_asset_total_deposits(&asset), 1_000_000);
    
    // After second deposit
    client.deposit_collateral_asset(&user, &asset, &500_000);
    assert_eq!(client.get_asset_total_deposits(&asset), 1_500_000);
}

#[test]
fn test_get_remaining_capacity_calculates_correctly() {
    let (env, client, admin, user) = setup();
    let asset = Address::generate(&env);
    let cap = 1_000_000;
    
    register_asset_with_cap(&client, &admin, &asset, cap);
    
    // Initially full capacity
    let remaining = client.get_asset_remaining_deposit_capacity(&asset).unwrap();
    assert_eq!(remaining, cap);
    
    // After deposit
    client.deposit_collateral_asset(&user, &asset, &300_000);
    let remaining = client.get_asset_remaining_deposit_capacity(&asset).unwrap();
    assert_eq!(remaining, 700_000);
    
    // At cap
    client.deposit_collateral_asset(&user, &asset, &700_000);
    let remaining = client.get_asset_remaining_deposit_capacity(&asset).unwrap();
    assert_eq!(remaining, 0);
}

#[test]
fn test_remaining_capacity_never_negative() {
    let (env, client, admin, user) = setup();
    let asset = Address::generate(&env);
    let initial_cap = 2_000_000;
    
    register_asset_with_cap(&client, &admin, &asset, initial_cap);
    
    // Deposit 1.5M
    client.deposit_collateral_asset(&user, &asset, &1_500_000);
    
    // Decrease cap below current deposits
    let new_cap = 1_000_000;
    let params = create_asset_params(&env, new_cap);
    client.set_asset_params(&asset, &params);
    
    // Remaining capacity should be 0, not negative
    let remaining = client.get_asset_remaining_deposit_capacity(&asset).unwrap();
    assert_eq!(remaining, 0);
}

// ============================================================================
// Error Handling Tests
// ============================================================================

#[test]
fn test_get_cap_for_unregistered_asset_fails() {
    let (env, client, _admin, _user) = setup();
    let unregistered_asset = Address::generate(&env);
    
    let result = client.try_get_asset_deposit_cap(&unregistered_asset);
    assert_eq!(result, Err(Ok(CrossAssetError::AssetNotSupported)));
}

#[test]
fn test_deposit_to_inactive_asset_fails() {
    let (env, client, admin, user) = setup();
    let asset = Address::generate(&env);
    
    client.register_asset(&admin, &asset);
    let mut params = create_asset_params(&env, 1_000_000);
    params.is_active = false;
    client.set_asset_params(&asset, &params);
    
    let result = client.try_deposit_collateral_asset(&user, &asset, &100_000);
    assert_eq!(result, Err(Ok(CrossAssetError::AssetNotSupported)));
}

// ============================================================================
// Overflow Protection Tests
// ============================================================================

#[test]
fn test_deposit_overflow_protection() {
    let (env, client, admin, user) = setup();
    let asset = Address::generate(&env);
    
    // Set very high cap
    register_asset_with_cap(&client, &admin, &asset, i128::MAX);
    
    // Deposit near max
    client.deposit_collateral_asset(&user, &asset, &(i128::MAX - 1000));
    
    // Try to deposit more - should fail with overflow
    let result = client.try_deposit_collateral_asset(&user, &asset, &2000);
    assert_eq!(result, Err(Ok(CrossAssetError::Overflow)));
}
