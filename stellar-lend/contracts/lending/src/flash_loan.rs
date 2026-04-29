use soroban_sdk::{contracterror, contracttype, token, Address, Bytes, Env, IntoVal, Symbol};

use crate::constants::{BPS_SCALE, MAX_FLASH_LOAN_FEE_BPS};

/// Errors that can occur during flash loan operations
pub use crate::errors::FlashLoanError;

/// Storage keys for flash loan data
#[contracttype]
#[derive(Clone)]
pub enum FlashLoanDataKey {
    FlashLoanFeeBps,
    ReentrancyGuard,
}

const MAX_FEE_BPS: i128 = MAX_FLASH_LOAN_FEE_BPS; // 10% maximum fee

/// Initiate a flash loan
///
/// # Arguments
/// * `env` - The contract environment
/// * `receiver` - The address of the contract receiving the funds and implementing the callback
/// * `asset` - The address of the token to borrow
/// * `amount` - The amount to borrow
/// * `params` - Arbitrary data to pass to the receiver's callback
pub fn flash_loan(
    env: &Env,
    receiver: Address,
    asset: Address,
    amount: i128,
    params: Bytes,
) -> Result<(), FlashLoanError> {
    if amount <= 0 {
        return Err(FlashLoanError::InvalidAmount);
    }

    // 1. Acquire Reentrancy Guard (Temporary Storage Lock)
    let _guard =
        crate::reentrancy::ReentrancyGuard::new(env).map_err(|_| FlashLoanError::Reentrancy)?;

    let fee = calculate_fee(env, amount);

    // 2. Record initial protocol state
    let token_client = token::Client::new(env, &asset);
    let initial_balance = token_client.balance(&env.current_contract_address());
    let initial_total_debt = crate::borrow::get_total_debt(env);
    let initial_total_deposits = crate::deposit::get_total_deposits(env);

    // 3. Transfer funds to the receiver
    token_client.transfer(&env.current_contract_address(), &receiver, &amount);

    // 4. Execute callback on receiver
    let callback_result: bool = env.invoke_contract(
        &receiver,
        &Symbol::new(env, "on_flash_loan"),
        (
            env.current_contract_address(),
            asset.clone(),
            amount,
            fee,
            params,
        )
            .into_val(env),
    );

    if !callback_result {
        return Err(FlashLoanError::CallbackFailed);
    }

    // 5. Verify repayment and state invariants
    let final_balance = token_client.balance(&env.current_contract_address());
    let final_total_debt = crate::borrow::get_total_debt(env);
    let final_total_deposits = crate::deposit::get_total_deposits(env);

    // Repayment must cover principal + fee
    if final_balance < initial_balance + fee {
        return Err(FlashLoanError::InsufficientRepayment);
    }

    // Protocol state must not have been mutated during the callback (Reentrancy Protection)
    if final_total_debt != initial_total_debt || final_total_deposits != initial_total_deposits {
        return Err(FlashLoanError::Reentrancy);
    }

    // 6. Enforce borrow cap post-loan accounting
    // This check prevents flash loans from being used to temporarily exceed the cap during
    // the callback and then repay, leaving the system in a cap-compliant state that was only
    // transiently non-compliant. By checking after repayment and reentrancy protection, we
    // ensure the net effect of the flash-loan interaction respects the cap.
    // Invariant: total borrowed for any capped asset never exceeds the cap at transaction end.
    enforce_borrow_cap_post_loan(env, &asset)?;

    Ok(())
}

/// Enforces borrow cap after flash-loan accounting.
///
/// Verifies that the net effect of the flash-loan interaction does not cause the asset's
/// total borrowed to exceed its borrow cap. This is called after repayment and reentrancy
/// checks to ensure the cap is enforced at transaction end.
///
/// # Arguments
/// * `env` - The contract environment
/// * `asset` - The asset that was flash-loaned
///
/// # Errors
/// * `FlashLoanError::BorrowCapExceeded`: If the asset's total borrowed exceeds its cap
fn enforce_borrow_cap_post_loan(env: &Env, asset: &Address) -> Result<(), FlashLoanError> {
    // Attempt to get asset params from cross-asset module
    if let Ok(params) = crate::cross_asset::get_asset_params_internal(env, asset) {
        if params.borrow_cap > 0 {
            let total_debt = crate::cross_asset::get_total_asset_debt_internal(env, asset);
            if total_debt > params.borrow_cap {
                return Err(FlashLoanError::BorrowCapExceeded);
            }
        }
    }
    Ok(())
}

/// Calculate the fee for a flash loan.
///
/// ## Rounding Semantics
/// `fee = amount * fee_bps / BPS_SCALE` — integer division truncates toward zero.
/// For small `amount` values the fee rounds down to zero.  The minimum amount
/// that yields a non-zero fee at `f` bps is `ceil(BPS_SCALE / f)`.
///
/// ## Fee-Splitting Note (Security)
/// Splitting one large loan into N smaller sub-threshold calls can reduce total
/// fees to zero because each call rounds independently.  The reentrancy guard
/// prevents this within a single transaction; operators should set
/// `min_borrow_amount` ≥ `ceil(BPS_SCALE / fee_bps)` to block sub-threshold
/// calls across separate transactions.
///
/// Overflow is handled by `saturating_mul` / `saturating_div`: if
/// `amount * fee_bps` overflows `i128`, the result saturates to `i128::MAX`
/// and then divides by `BPS_SCALE`, so the fee remains positive and bounded.
fn calculate_fee(env: &Env, amount: i128) -> i128 {
    let fee_bps = get_flash_loan_fee_bps(env);
    amount.saturating_mul(fee_bps).saturating_div(BPS_SCALE)
}

/// Set the flash loan fee in basis points
pub fn set_flash_loan_fee_bps(env: &Env, fee_bps: i128) -> Result<(), FlashLoanError> {
    if !(0..=MAX_FEE_BPS).contains(&fee_bps) {
        return Err(FlashLoanError::InvalidFee);
    }
    env.storage()
        .persistent()
        .set(&FlashLoanDataKey::FlashLoanFeeBps, &fee_bps);
    Ok(())
}

/// Get the current flash loan fee in basis points
pub fn get_flash_loan_fee_bps(env: &Env) -> i128 {
    env.storage()
        .persistent()
        .get(&FlashLoanDataKey::FlashLoanFeeBps)
        .unwrap_or(5) // Default 5 bps (0.05%)
}
