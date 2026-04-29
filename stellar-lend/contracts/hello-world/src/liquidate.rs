//! # Liquidation Module
//!
//! Verification-prep notes for formal methods:
//! - Precondition checks reject zero/negative debt input, paused operation states,
//!   and healthy borrower positions.
//! - Effects update borrower debt/collateral before token transfers (CEI ordering).
//! - External interaction points are explicit: oracle price reads, token decimals,
//!   and SRC-20 transfers for debt and collateral settlement.
//! - Arithmetic uses checked operators or I256 intermediate math on scaled values.

#![allow(unused)]
use crate::events::{emit_liquidation, LiquidationEvent};
use crate::amm::{self, SwapParams, AmmError};
use soroban_sdk::token::Client as TokenClient;
use soroban_sdk::{contracterror, token, Address, Env, IntoVal, Map, Symbol, Val, Vec, I256};

use crate::deposit::{
    add_activity_log, emit_analytics_updated_event, emit_position_updated_event,
    emit_user_activity_tracked_event, AssetParams, DepositDataKey, Position, ProtocolAnalytics,
    UserAnalytics,
};
use crate::oracle::get_price;
use crate::risk_management::{
    is_emergency_paused, is_operation_paused, require_operation_not_paused, RiskManagementError,
};
use crate::risk_params::{
    can_be_liquidated, get_liquidation_incentive_amount, get_max_liquidatable_amount,
    get_risk_params,
};

const MAX_DECIMALS_FOR_SCALING: u32 = 18;

/// Errors that can occur during liquidation operations
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum LiquidationError {
    /// Liquidation amount must be greater than zero
    InvalidAmount = 1,
    /// Asset address is invalid
    InvalidAsset = 2,
    /// Position is not undercollateralized
    NotLiquidatable = 3,
    /// Liquidation operations are currently paused
    LiquidationPaused = 4,
    /// Liquidation amount exceeds maximum allowed (close factor)
    ExceedsCloseFactor = 5,
    /// Insufficient balance to liquidate
    InsufficientBalance = 6,
    /// Overflow occurred during calculation
    Overflow = 7,
    /// Reentrancy detected during liquidation
    Reentrancy = 8,
    /// Invalid collateral asset
    InvalidCollateralAsset = 9,
    /// Invalid debt asset
    InvalidDebtAsset = 10,
    /// Price not available for asset
    PriceNotAvailable = 11,
    /// Protocol is in read-only mode
    ReadOnlyMode = 12,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Executes a normal liquidation of `borrower`'s `borrow_asset` position.
///
/// The liquidator specifies how much debt they wish to repay (`repay_amount`).
/// The function caps this at `CLOSE_FACTOR_BPS` of the outstanding borrow and
/// ensures the position is actually unhealthy before proceeding.
pub fn liquidate(
    env: &Env,
    liquidator: Address,
    borrower: Address,
    debt_asset: Option<Address>,
    collateral_asset: Option<Address>,
    debt_amount: i128,
) -> Result<(i128, i128, i128), LiquidationError> {
    let (actual_debt_liquidated, collateral_seized, incentive_amount) = execute_liquidation_logic(
        env,
        &liquidator,
        &borrower,
        &debt_asset,
        &collateral_asset,
        debt_amount,
    )?;

    // 9. EXTERNAL INTERACTIONS (TRANSFERS)
    // Transfers are performed LAST to follow CEI pattern

    let debt_addr = match &debt_asset {
        Some(ref addr) => addr.clone(),
        None => get_native_asset_address(env)?,
    };
    let debt_client = TokenClient::new(env, &debt_addr);
    debt_client.transfer_from(
        &env.current_contract_address(),
        &liquidator,
        &env.current_contract_address(),
        &actual_debt_liquidated,
    );

    let col_addr = match &collateral_asset {
        Some(ref addr) => addr.clone(),
        None => get_native_asset_address(env)?,
    };
    let col_client = TokenClient::new(env, &col_addr);
    col_client.transfer(
        &env.current_contract_address(),
        &liquidator,
        &collateral_seized,
    );

    // 10. EMIT EVENTS
    emit_liquidation_events(
        env,
        &liquidator,
        &borrower,
        &debt_asset,
        &collateral_asset,
        actual_debt_liquidated,
        collateral_seized,
        incentive_amount,
    );

    Ok((actual_debt_liquidated, collateral_seized, incentive_amount))
}

/// Internal function to execute the core liquidation logic without transfers
fn execute_liquidation_logic(
    env: &Env,
    liquidator: &Address,
    borrower: &Address,
    debt_asset: &Option<Address>,
    collateral_asset: &Option<Address>,
    debt_amount: i128,
) -> Result<(i128, i128, i128), LiquidationError> {
    // 1. Initial validation
    if debt_amount <= 0 {
        return Err(LiquidationError::InvalidAmount);
    }

    // Explicit authorization check for liquidator
    liquidator.require_auth();

    // Reentrancy guard for all liquidation external-call paths.
    let _guard =
        crate::reentrancy::ReentrancyGuard::new(env).map_err(|_| LiquidationError::Reentrancy)?;

    // 2. Authorization and Pause Checks
    // 2a. Read-only mode (highest precedence)
    if crate::risk_management::is_read_only_mode(env) {
        return Err(LiquidationError::ReadOnlyMode);
    }

    // 2b. Emergency pause
    if is_emergency_paused(env) {
        return Err(LiquidationError::LiquidationPaused);
    }

    // 2c. Per-operation pause
    require_operation_not_paused(env, Symbol::new(env, "pause_liquidate"))
        .map_err(|_| LiquidationError::LiquidationPaused)?;

    // 3. Load Borrower State
    let position_key = DepositDataKey::Position(borrower.clone());
    let mut position = env
        .storage()
        .persistent()
        .get::<DepositDataKey, Position>(&position_key)
        .ok_or(LiquidationError::NotLiquidatable)?;

    // 4. Load Collateral State
    let collateral_key = DepositDataKey::CollateralBalance(borrower.clone());
    let borrower_collateral = env
        .storage()
        .persistent()
        .get::<DepositDataKey, i128>(&collateral_key)
        .unwrap_or(0);

    // 5. Fetch Prices and Decimals (Interactions - allowed here as they don't modify state)
    let (debt_price, collateral_price) =
        get_liquidation_prices(env, &debt_asset, &collateral_asset)?;
    let debt_decimals = get_asset_decimals(env, &debt_asset);
    let collateral_decimals = get_asset_decimals(env, &collateral_asset);

    if debt_decimals > MAX_DECIMALS_FOR_SCALING || collateral_decimals > MAX_DECIMALS_FOR_SCALING {
        return Err(LiquidationError::Overflow);
    }

    // Guard: no new liquidations during shutdown (use emergency_liquidate).
    if storage::is_shutdown(env) {
        return Err(LendingError::EmergencyShutdown);
    }

    let borrow_market = storage::get_market(env, borrow_asset)?;
    if !borrow_market.is_active {
        return Err(LendingError::MarketNotFound);
    }

    // 7. CALCULATE SEIZURE WITH PRECISION MATH
    let incentive_bps = get_risk_params(env)
        .map(|p| p.liquidation_incentive)
        .unwrap_or(1000);
    let bonus_multiplier = 10000i128
        .checked_add(incentive_bps)
        .ok_or(LiquidationError::Overflow)?;

    let amount_256 = I256::from_i128(env, actual_debt_liquidated);
    let debt_price_256 = I256::from_i128(env, debt_price);
    let bonus_multiplier_256 = I256::from_i128(env, bonus_multiplier);
    let collateral_price_256 = I256::from_i128(env, collateral_price);
    let bps_scale_256 = I256::from_i128(env, 10000);

    let debt_scale_val = 10i128.pow(debt_decimals);
    let col_scale_val = 10i128.pow(collateral_decimals);
    let debt_scale_256 = I256::from_i128(env, debt_scale_val);
    let col_scale_256 = I256::from_i128(env, col_scale_val);

    let numerator_256 = amount_256
        .mul(&debt_price_256)
        .mul(&bonus_multiplier_256)
        .mul(&col_scale_256);

    let denominator_256 = collateral_price_256
        .mul(&bps_scale_256)
        .mul(&debt_scale_256);

    let seized_256 = numerator_256.div(&denominator_256);
    let mut collateral_seized = seized_256.to_i128().ok_or(LiquidationError::Overflow)?;

    collateral_seized = collateral_seized.min(borrower_collateral);

    let incentive_amount =
        get_liquidation_incentive_amount(env, actual_debt_liquidated).unwrap_or(0);

    // 8. UPDATE STORAGE (EFFECTS)
    let total_interest_to_repay = position
        .borrow_interest
        .checked_add(
            current_total_debt
                .checked_sub(position.debt + position.borrow_interest)
                .unwrap_or(0),
        )
        .ok_or(LiquidationError::Overflow)?;

    if actual_debt_liquidated <= total_interest_to_repay {
        position.borrow_interest = total_interest_to_repay - actual_debt_liquidated;
    } else {
        let remaining_to_principal = actual_debt_liquidated - total_interest_to_repay;
        position.borrow_interest = 0;
        position.debt = position
            .debt
            .checked_sub(remaining_to_principal)
            .unwrap_or(0);
    }

    // Verify position is unhealthy.
    check_position_unhealthy(env, borrower, borrow_asset, collateral_asset)?;

    // Apply close factor cap.
    let max_repay = user_borrow
        .checked_mul(CLOSE_FACTOR_BPS)
        .ok_or(LendingError::InvalidAmount)?
        / oracle::BPS_DENOM;
    let actual_repay = repay_amount.min(max_repay);

    record_liquidation_analytics(env, actual_debt_liquidated, collateral_seized)
        .map_err(|_| LiquidationError::Overflow)?;

    Ok((actual_debt_liquidated, collateral_seized, incentive_amount))
}

fn emit_liquidation_events(
    env: &Env,
    liquidator: &Address,
    borrower: &Address,
    debt_asset: &Option<Address>,
    collateral_asset: &Option<Address>,
    actual_debt_liquidated: i128,
    collateral_seized: i128,
    incentive_amount: i128,
) {
    let (debt_price, collateral_price) =
        get_liquidation_prices(env, debt_asset, collateral_asset).unwrap_or((0, 0));

    let position_key = DepositDataKey::Position(borrower.clone());
    let position = env
        .storage()
        .persistent()
        .get::<DepositDataKey, Position>(&position_key)
        .unwrap();

    emit_liquidation(
        env,
        LiquidationEvent {
            liquidator: liquidator.clone(),
            borrower: borrower.clone(),
            debt_asset: debt_asset.clone(),
            collateral_asset: collateral_asset.clone(),
            debt_liquidated: actual_debt_liquidated,
            collateral_seized,
            incentive_amount,
            debt_price,
            collateral_price,
            timestamp: position.last_accrual_time,
        },
    );

    emit_position_updated_event(
        env,
        borrower,
        &position,
        Symbol::new(env, "liquidate"),
        position.last_accrual_time,
    );
    add_activity_log(
        env,
        borrower,
        Symbol::new(env, "liquidate"),
        actual_debt_liquidated,
        debt_asset.clone(),
        position.last_accrual_time,
    )
    .ok();
}

    // Formal-verification postcondition note:
    // liquidation cannot increase borrower debt/collateral and must respect caps.
    /* debug_assert!(fv_liquidate_postconditions(
        &fv_snapshot,
        &liquidator_position_before,
        &borrower_position,
        &liquidator_position,
        repay_amount,
    )); */

    Ok((actual_debt_liquidated, collateral_seized, incentive_amount))
}

/// Full liquidation regardless of close factor.  Used by governance during
/// emergency shutdown.  Always writes off any residual shortfall.
pub fn emergency_liquidate(
    env: &Env,
    borrower: &Address,
    borrow_asset: &Address,
    collateral_asset: &Address,
) -> Result<LiquidationResult, LendingError> {
    let user_borrow = storage::get_user_borrow(env, borrower, borrow_asset);
    if user_borrow == 0 {
        return Ok(LiquidationResult {
            collateral_seized: 0,
            debt_repaid: 0,
            bad_debt_event: None,
        });
    }

    let user_collateral = storage::get_user_deposit(env, borrower, collateral_asset);

    // Seize all collateral.
    storage::set_user_deposit(env, borrower, collateral_asset, 0);
    // NOTE: do NOT zero user_borrow here — record_bad_debt reads it to
    // correctly decrement total_borrows.  It will zero it as part of I-6.

    let mut col_mkt = storage::get_market(env, collateral_asset)?;
    col_mkt.total_deposits = (col_mkt.total_deposits - user_collateral).max(0);
    storage::set_market(env, collateral_asset, &col_mkt);

    // Compute residual in USD terms.
    let collateral_usd = oracle::usd_value(env, collateral_asset, user_collateral)?;
    let borrow_usd = oracle::usd_value(env, borrow_asset, user_borrow)?;
    let residual = (borrow_usd - collateral_usd).max(0);

    // record_bad_debt zeros the user borrow, decrements total_borrows,
    // and writes off the residual against reserves / bad_debt.
    let bad_debt_event = if residual > 0 {
        Some(bad_debt_accounting::record_bad_debt(
            env,
            borrower,
            borrow_asset,
            residual,
            user_collateral,
        )?)
    } else {
        // No shortfall: manually zero the position and decrement totals.
        let user_borrow_remaining = storage::get_user_borrow(env, borrower, borrow_asset);
        storage::set_user_borrow(env, borrower, borrow_asset, 0);
        let mut borrow_mkt = storage::get_market(env, borrow_asset)?;
        borrow_mkt.total_borrows = (borrow_mkt.total_borrows - user_borrow_remaining).max(0);
        storage::set_market(env, borrow_asset, &borrow_mkt);
        None
    };

    Ok(LiquidationResult {
        collateral_seized: user_collateral,
        debt_repaid: user_borrow,
        bad_debt_event,
    })
}

/// Snapshot of state taken before liquidation for formal-verification hooks.
#[derive(Clone, Copy)]
struct LiquidationSpecSnapshot {
    total_debt_before: i128,
    collateral_before: i128,
}

#[inline(always)]
fn fv_liquidate_preconditions(debt_amount: i128) -> bool {
    debt_amount > 0
}

#[inline(always)]
fn fv_liquidate_postconditions(
    snapshot: &LiquidationSpecSnapshot,
    position: &Position,
    debt_repaid: i128,
    collateral_seized: i128,
) -> bool {
    let debt_reduced = snapshot.total_debt_before.saturating_sub(debt_repaid);
    position.debt + position.borrow_interest <= debt_reduced
        && position.collateral <= snapshot.collateral_before
        && collateral_seized <= snapshot.collateral_before
}

#[cfg(test)]
mod verification_hooks_tests {
    use super::*;

    #[test]
    fn liquidate_hooks_accept_valid_transition() {
        let snapshot = LiquidationSpecSnapshot {
            total_debt_before: 1_000,
            collateral_before: 800,
        };
        let position = Position {
            collateral: 600,
            debt: 700,
            borrow_interest: 100,
            last_accrual_time: 0,
        };

        assert!(fv_liquidate_preconditions(100));
        assert!(fv_liquidate_postconditions(&snapshot, &position, 200, 200));
    }

    #[test]
    fn liquidate_hooks_reject_invalid_transition() {
        let snapshot = LiquidationSpecSnapshot {
            total_debt_before: 1_000,
            collateral_before: 800,
        };
        let position = Position {
            collateral: 900,
            debt: 900,
            borrow_interest: 200,
            last_accrual_time: 0,
        };

        assert!(!fv_liquidate_preconditions(0));
        assert!(!fv_liquidate_postconditions(
            &snapshot, &position, 1_100, 900
        ));
    }
    Ok(())
}

fn compute_collateral_seized(
    env: &Env,
    borrow_asset: &Address,
    collateral_asset: &Address,
    repay_amount: i128,
    bonus_bps: i128,
) -> Result<i128, LendingError> {
    let borrow_price = oracle::get_price(env, borrow_asset)?;
    let collateral_price = oracle::get_price(env, collateral_asset)?;

    // repay_amount expressed in collateral units, grossed up by bonus.
    let seized = repay_amount
        .checked_mul(borrow_price)
        .ok_or(LendingError::InvalidAmount)?
        .checked_mul(bonus_bps)
        .ok_or(LendingError::InvalidAmount)?
        / (collateral_price * oracle::BPS_DENOM);

    Ok(seized)
}