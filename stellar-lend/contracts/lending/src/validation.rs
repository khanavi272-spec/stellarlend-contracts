//! # Parameter Validation Helpers
//!
//! Provides centralized logic for enforcing strict bounds on admin-set parameters.
//! Prevents unsafe configurations that could lead to protocol instability.

use soroban_sdk::Env;
use crate::constants::BPS_SCALE;
use crate::errors::BorrowError;

/// Enforces that a value expressed in basis points is within [0, 10000].
pub fn assert_bps_range(bps: i128) -> Result<(), BorrowError> {
    if !(0..=BPS_SCALE).contains(&bps) {
        return Err(BorrowError::InvalidParameterRange);
    }
    Ok(())
}

/// Enforces that a value is non-negative.
pub fn assert_positive(value: i128) -> Result<(), BorrowError> {
    if value < 0 {
        return Err(BorrowError::InvalidParameterRange);
    }
    Ok(())
}

/// Enforces that LTV is less than or equal to the liquidation threshold.
/// This prevents positions from being immediately liquidatable.
pub fn assert_ltv_threshold(ltv: i128, threshold: i128) -> Result<(), BorrowError> {
    assert_bps_range(ltv)?;
    assert_bps_range(threshold)?;
    if ltv > threshold {
        return Err(BorrowError::InvalidParameterRange);
    }
    Ok(())
}

/// Enforces that a rate floor is less than or equal to a rate ceiling.
pub fn assert_rate_bounds(floor: i128, ceiling: i128) -> Result<(), BorrowError> {
    assert_bps_range(floor)?;
    assert_bps_range(ceiling)?;
    if floor > ceiling {
        return Err(BorrowError::InvalidParameterRange);
    }
    Ok(())
}

/// Enforces that a time interval is positive and within a reasonable max (e.g. 30 days).
pub fn assert_time_interval(seconds: u64, min: u64, max: u64) -> Result<(), BorrowError> {
    if seconds < min || seconds > max {
        return Err(BorrowError::InvalidParameterRange);
    }
    Ok(())
}
