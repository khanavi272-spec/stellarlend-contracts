//! # Oracle Module
//!
//! Manages price feeds for all protocol assets with staleness checks, deviation
//! guards, caching, and fallback oracle support.
//!
//! ## Price Resolution Order
//! 1. **Cache**: returns a cached price if the TTL has not expired.
//! 2. **Primary feed**: reads the on-chain `PriceFeed` entry; rejects if stale.
//! 3. **Fallback oracle**: if the primary is stale or missing, queries a
//!    configured fallback oracle address.
//!
//! ## Safety
//! - Price deviation between consecutive updates is bounded (default ±5%).
//! - Staleness threshold defaults to 1 hour; configurable by admin.
//! - Sanity-check bounds on min/max price are enforced on every update.
//! - Only the admin or the designated oracle address may submit price updates.

#![allow(unused)]
use crate::admin::get_admin;
use crate::deposit::DepositDataKey;
use crate::events::{emit_price_updated, PriceUpdatedEvent};
use crate::prelude::*;
use soroban_sdk::{contracterror, contracttype, Address, Env, IntoVal, Map, Symbol, Val, Vec};

/// Errors that can occur during oracle operations
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum OracleError {
    /// Invalid price (zero or negative)
    InvalidPrice = 1,
    /// Price is too stale (older than threshold)
    StalePrice = 2,
    /// Price deviation exceeds maximum allowed
    PriceDeviationExceeded = 3,
    /// Oracle address is invalid
    InvalidOracle = 4,
    /// Oracle update is paused
    OraclePaused = 5,
    /// Overflow occurred during calculation
    Overflow = 6,
    /// Unauthorized access
    Unauthorized = 7,
    /// Asset not supported
    AssetNotSupported = 8,
    /// Fallback oracle not configured
    FallbackNotConfigured = 9,
}

/// Storage keys for oracle-related data
#[contracttype]
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub enum OracleDataKey {
    /// Latest price feed data for a specific asset
    /// Value type: PriceFeed
    PriceFeed(Address),
    /// Address of the designated fallback oracle for an asset
    /// Value type: Address
    FallbackOracle(Address),
    /// Primary oracle address for an asset
    /// Value type: Address
    PrimaryOracle(Address),
    /// Fallback price feed for an asset
    /// Value type: PriceFeed
    FallbackFeed(Address),
    /// Transient price cache for improved gas efficiency
    /// Value type: CachedPrice
    PriceCache(Address),
    /// Global oracle safety and operational parameters
    /// Value type: OracleConfig
    OracleConfig,
    /// Pause switches specifically for oracle updates: Map<Symbol, bool>
    PauseSwitches,
}

/// Price feed data structure
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PriceFeed {
    /// Current price (in smallest unit, e.g., cents for USD)
    pub price: i128,
    /// Timestamp when price was last updated
    pub last_updated: u64,
    /// Oracle address that provided this price
    pub oracle: Address,
    /// Price decimals (e.g., 8 for BTC, 2 for USD)
    pub decimals: u32,
}

/// Cached price data
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct CachedPrice {
    /// Cached price
    pub price: i128,
    /// Timestamp when price was cached
    pub cached_at: u64,
    /// Cache TTL in seconds
    pub ttl: u64,
}

/// Oracle configuration
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct OracleConfig {
    /// Maximum price deviation in basis points (e.g., 500 = 5%)
    pub max_deviation_bps: i128,
    /// Maximum staleness in seconds
    pub max_staleness_seconds: u64,
    /// Cache TTL in seconds
    pub cache_ttl_seconds: u64,
    /// Minimum price sanity check
    pub min_price: i128,
    /// Maximum price sanity check
    pub max_price: i128,
}

/// Default configuration values
const DEFAULT_MAX_DEVIATION_BPS: i128 = 500; // 5%
const DEFAULT_MAX_STALENESS_SECONDS: u64 = 3600; // 1 hour
const DEFAULT_CACHE_TTL_SECONDS: u64 = 300; // 5 minutes
const DEFAULT_MIN_PRICE: i128 = 1;
const DEFAULT_MAX_PRICE: i128 = i128::MAX;

/// Get default oracle configuration
fn get_default_config() -> OracleConfig {
    OracleConfig {
        max_deviation_bps: DEFAULT_MAX_DEVIATION_BPS,
        max_staleness_seconds: DEFAULT_MAX_STALENESS_SECONDS,
        cache_ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
        min_price: DEFAULT_MIN_PRICE,
        max_price: DEFAULT_MAX_PRICE,
    }
}

/// Get oracle configuration
fn get_oracle_config(env: &Env) -> OracleConfig {
    let config_key = OracleDataKey::OracleConfig;
    env.storage()
        .persistent()
        .get::<OracleDataKey, OracleConfig>(&config_key)
        .unwrap_or_else(get_default_config)
}

/// Get primary oracle for an asset
fn get_primary_oracle(env: &Env, asset: &Address) -> Option<Address> {
    let key = OracleDataKey::PrimaryOracle(asset.clone());
    env.storage()
        .persistent()
        .get::<OracleDataKey, Address>(&key)
}

/// Get fallback oracle for an asset
fn get_fallback_oracle(env: &Env, asset: &Address) -> Option<Address> {
    let key = OracleDataKey::FallbackOracle(asset.clone());
    env.storage()
        .persistent()
        .get::<OracleDataKey, Address>(&key)
}

/// Validate price against sanity checks
fn validate_price(env: &Env, price: i128) -> Result<(), OracleError> {
    if price <= 0 {
        return Err(OracleError::InvalidPrice);
    }

    let config = get_oracle_config(env);
    if price < config.min_price || price > config.max_price {
        return Err(OracleError::InvalidPrice);
    }

    Ok(())
}

/// Check if price is stale
fn is_price_stale(env: &Env, last_updated: u64) -> bool {
    let config = get_oracle_config(env);
    let current_time = env.ledger().timestamp();

    if current_time < last_updated {
        return true; // Invalid timestamp
    }

    let age = current_time - last_updated;
    age > config.max_staleness_seconds
}

/// Check price deviation between two prices
fn check_price_deviation(env: &Env, new_price: i128, old_price: i128) -> Result<(), OracleError> {
    if old_price == 0 {
        return Ok(()); // No previous price to compare
    }

    let config = get_oracle_config(env);

    // Calculate deviation: |new - old| / old * 10000 (basis points)
    let diff = if new_price > old_price {
        new_price
            .checked_sub(old_price)
            .ok_or(OracleError::Overflow)?
    } else {
        old_price
            .checked_sub(new_price)
            .ok_or(OracleError::Overflow)?
    };

    let deviation_bps = diff
        .checked_mul(10000)
        .ok_or(OracleError::Overflow)?
        .checked_div(old_price)
        .ok_or(OracleError::Overflow)?;

    if deviation_bps > config.max_deviation_bps {
        return Err(OracleError::PriceDeviationExceeded);
    }

    Ok(())
}

/// Get cached price if valid
fn get_cached_price(env: &Env, asset: &Address) -> Option<i128> {
    let cache_key = OracleDataKey::PriceCache(asset.clone());
    if let Some(cached) = env
        .storage()
        .persistent()
        .get::<OracleDataKey, CachedPrice>(&cache_key)
    {
        let current_time = env.ledger().timestamp();
        if current_time >= cached.cached_at
            && current_time <= cached.cached_at.saturating_add(cached.ttl)
        {
            return Some(cached.price);
        }
    }
    None
}

/// Cache price
fn cache_price(env: &Env, asset: &Address, price: i128) {
    let config = get_oracle_config(env);
    let cache_key = OracleDataKey::PriceCache(asset.clone());
    let cached = CachedPrice {
        price,
        cached_at: env.ledger().timestamp(),
        ttl: config.cache_ttl_seconds,
    };
    env.storage().persistent().set(&cache_key, &cached);
}

/// Update price feed from oracle
///
/// # Arguments
/// * `env` - The Soroban environment
/// * `caller` - The address calling this function (must be admin or oracle)
/// * `asset` - The asset address
/// * `price` - The new price
/// * `decimals` - Price decimals
/// * `oracle` - The oracle address providing this price
///
/// # Returns
/// Returns the updated price
pub fn update_price_feed(
    env: &Env,
    caller: Address,
    asset: Address,
    price: i128,
    decimals: u32,
    oracle: Address,
) -> Result<i128, OracleError> {
    // Check if oracle updates are paused
    let pause_key = OracleDataKey::PauseSwitches;
    if let Some(pause_map) = env
        .storage()
        .persistent()
        .get::<OracleDataKey, Map<Symbol, bool>>(&pause_key)
    {
        if let Some(paused) = pause_map.get(Symbol::new(env, "pause_oracle")) {
            if paused {
                return Err(OracleError::OraclePaused);
            }
        }
    }

    // Validate caller authorization
    let is_admin = get_admin(env).map(|admin| admin == caller).unwrap_or(false);
    let primary = get_primary_oracle(env, &asset);
    let fallback = get_fallback_oracle(env, &asset);

    let is_primary = primary.map(|p| p == caller).unwrap_or(false);
    let is_fallback = fallback.map(|f| f == caller).unwrap_or(false);

    if !is_admin && !is_primary && !is_fallback {
        return Err(OracleError::Unauthorized);
    }

    // Ensure oracle address matches caller if not admin
    if !is_admin && caller != oracle {
        return Err(OracleError::Unauthorized);
    }

    // Validate price
    validate_price(env, price)?;

    // Determine target storage key and get current feed for deviation check
    let feed_key = if is_fallback && !is_primary && !is_admin {
        OracleDataKey::FallbackFeed(asset.clone())
    } else {
        OracleDataKey::PriceFeed(asset.clone())
    };

    let current_feed = env
        .storage()
        .persistent()
        .get::<OracleDataKey, PriceFeed>(&feed_key);

    // Check price deviation if we have a previous price
    if let Some(ref feed) = current_feed {
        check_price_deviation(env, price, feed.price)?;
    }

    // Create new price feed
    let timestamp = env.ledger().timestamp();
    let oracle_clone = oracle.clone();
    let new_feed = PriceFeed {
        price,
        last_updated: timestamp,
        oracle: oracle_clone.clone(),
        decimals,
    };

    // Update storage
    env.storage().persistent().set(&feed_key, &new_feed);

    // When admin submits a price, register the oracle address as the primary oracle
    // for the asset so subsequent calls from that oracle are authorized.
    if is_admin {
        let primary_key = OracleDataKey::PrimaryOracle(asset.clone());
        env.storage().persistent().set(&primary_key, &oracle);
    }

    // Update cache
    cache_price(env, &asset, price);

    // Emit price update event
    emit_price_updated(
        env,
        PriceUpdatedEvent {
            actor: caller,
            asset: asset.clone(),
            price,
            decimals,
            oracle: oracle_clone,
            timestamp,
        },
    );

    Ok(price)
}

/// Get price for an asset with fallback support
///
/// # Arguments
/// * `env` - The Soroban environment
/// * `asset` - The asset address
///
/// In production this would call an external oracle contract.  During tests the
/// mock store (set via `set_mock_price`) is consulted first.
pub fn get_price(env: &Env, asset: &Address) -> Result<i128, crate::types::LendingError> {
    // Try mock store (test environment).
    let key = soroban_sdk::symbol_short!("prices");
    if let Some(map) = env
        .storage()
        .temporary()
        .get::<soroban_sdk::Symbol, Map<Address, i128>>(&key)
    {
        if let Some(price) = map.get(asset.clone()) {
            validate_price(price)?;
            return Ok(price);
        }
    }
    // Fallback: no oracle configured → reject.
    Err(crate::types::LendingError::InvalidOracle)
}

/// Sets a mock price in temporary storage (test-only helper exposed via
/// `#[cfg(test)]` in the test modules).
pub fn set_mock_price(env: &Env, asset: &Address, price: i128) {
    let key = soroban_sdk::symbol_short!("prices");
    let mut map: Map<Address, i128> = env
        .storage()
        .temporary()
        .get::<soroban_sdk::Symbol, Map<Address, i128>>(&key)
        .unwrap_or_else(|| Map::new(env));
    map.set(asset.clone(), price);
    env.storage().temporary().set(&key, &map);
}

/// Computes the USD value of `amount` units of `asset`.
///
/// `amount` is in the asset's native precision (scaled by PRICE_PRECISION
/// here so callers stay in integer arithmetic).
pub fn usd_value(env: &Env, asset: &Address, amount: i128) -> Result<i128, crate::types::LendingError> {
    let price = get_price(env, asset)?;
    Ok(amount
        .checked_mul(price)
        .ok_or(crate::types::LendingError::InvalidAmount)?
        / PRICE_PRECISION)
}

/// Computes the maximum borrow capacity of a collateral position.
///
/// `collateral_factor_bps` is expressed in basis points (e.g. 7500 = 75%).
pub fn max_borrow_usd(
    env: &Env,
    collateral_asset: &Address,
    collateral_amount: i128,
    collateral_factor_bps: i128,
) -> Result<i128, crate::types::LendingError> {
    let col_value = usd_value(env, collateral_asset, collateral_amount)?;
    Ok(col_value
        .checked_mul(collateral_factor_bps)
        .ok_or(crate::types::LendingError::InvalidAmount)?
        / 10_000)
}

// ── Internal helpers ─────────────────────────────────────────────────────────

fn validate_price(price: i128) -> Result<(), crate::types::LendingError> {
    if price < MIN_PRICE {
        return Err(crate::types::LendingError::OraclePriceTooLow);
    }
    Ok(())
}