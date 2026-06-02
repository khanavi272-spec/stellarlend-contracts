#![no_std]

mod debt;
pub mod rate_model;
pub mod rounding_strategy;

#[cfg(test)]
mod interest_drift_regression_test;

use debt::{
    borrow_amount, effective_debt, load_debt, repay_amount, save_debt, DebtPosition,
    DEFAULT_APR_BPS,
};
use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, Address, Bytes, Env,
    IntoVal, Symbol, Val,
};

pub use stellar_lend_common::BPS_DENOM;

const PERSISTENT_TTL_LEDGERS: u32 = 1_000_000;
const DEFAULT_DEPOSIT_CAP: i128 = 1_000_000_000_000;

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataKey {
    Collateral(Address),
    Debt(Address),
    Balance(Address, Address),
    Treasury(Address),
    TotalDebt,
    TotalDeposits,
    DebtCeiling,
    DepositCap,
    FlashActive,
    FlashFeeBps,
    BorrowMinAmount,
    Admin,
    PendingAdmin,
    EmergencyState,
    Guardian,
    PauseState(PauseType),
    RateParams,
}

#[contractevent]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmergencyStateChangedEvent {
    pub old_state: EmergencyState,
    pub new_state: EmergencyState,
}

#[contractevent]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PauseStateChangedEvent {
    pub operation: PauseType,
    pub old_state: PauseState,
    pub new_state: PauseState,
}

#[contracttype]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmergencyState {
    Normal,
    Shutdown,
    Recovery,
}

#[contracttype]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseType {
    All,
    Deposit,
    Withdraw,
    Borrow,
    Repay,
    Liquidation,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PauseState {
    pub paused: bool,
    pub expires_at_ledger: u32,
}

pub enum ProtocolAction {
    Deposit,
    Withdraw,
    Borrow,
    Repay,
    Liquidate,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum LendingError {
    InvalidAmount = 1004,
    BelowMinimumBorrow = 1008,
    NotInitialized = 1009,
    AlreadyInitialized = 1010,
    InsufficientCollateral = 1011,
    DebtCeilingExceeded = 2001,
    DepositCapExceeded = 2002,
    Overflow = 2003,
    Unauthorized = 2004,
    InvalidFeeBps = 2005,
    PositionHealthy = 2006,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PositionSummary {
    pub collateral: i128,
    pub debt: i128,
    pub health_factor: i128,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolMetrics {
    pub total_supply: i128,
    pub total_borrow: i128,
    pub utilization_bps: i128,
    pub ledger: u32,
}

#[contract]
pub struct LendingContract;

#[contractimpl]
impl LendingContract {
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("AlreadyInitialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        set_emergency_state_internal(&env, EmergencyState::Normal);
    }

    pub fn get_admin(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Admin).unwrap()
    }

    pub fn propose_admin(env: Env, new_admin: Address) {
        let current_admin = Self::get_admin(env.clone());
        current_admin.require_auth();
        env.storage().instance().set(&DataKey::PendingAdmin, &new_admin);
    }

    pub fn accept_admin(env: Env) {
        let pending_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .expect("no pending admin");
        pending_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &pending_admin);
        env.storage().instance().remove(&DataKey::PendingAdmin);
    }

    pub fn set_guardian(env: Env, guardian: Address) {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        env.storage().instance().set(&DataKey::Guardian, &guardian);
    }

    pub fn get_guardian(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Guardian)
    }

    pub fn set_emergency_state(env: Env, new_state: EmergencyState) {
        let admin = Self::get_admin(env.clone());

        match new_state {
            EmergencyState::Shutdown => {
                let guardian_opt: Option<Address> =
                    env.storage().instance().get(&DataKey::Guardian);
                let authorized = match guardian_opt {
                    Some(ref guardian) => {
                        let try_guardian = env.auths().iter().any(|(a, _)| a == guardian);
                        let try_admin = env.auths().iter().any(|(a, _)| a == &admin);
                        if try_guardian {
                            guardian.require_auth();
                            true
                        } else if try_admin {
                            admin.require_auth();
                            true
                        } else {
                            false
                        }
                    }
                    None => {
                        admin.require_auth();
                        true
                    }
                };
                if !authorized {
                    panic!("Unauthorized");
                }
            }
            EmergencyState::Recovery | EmergencyState::Normal => {
                admin.require_auth();
            }
        }

        let old_state = get_emergency_state(&env);
        set_emergency_state_internal(&env, new_state);
        EmergencyStateChangedEvent { old_state, new_state }.publish(&env);
    }

    pub fn set_min_borrow(env: Env, min_borrow: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        env.storage().instance().set(&DataKey::BorrowMinAmount, &min_borrow);
        Ok(())
    }

    pub fn get_min_borrow(env: Env) -> i128 {
        env.storage().instance().get(&DataKey::BorrowMinAmount).unwrap_or(0)
    }

    pub fn set_flash_fee(env: Env, fee_bps: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        if fee_bps < 0 || fee_bps > 1000 {
            return Err(LendingError::InvalidFeeBps);
        }
        env.storage().instance().set(&DataKey::FlashFeeBps, &fee_bps);
        Ok(())
    }

    fn get_flash_fee_bps(env: &Env) -> i128 {
        env.storage().instance().get(&DataKey::FlashFeeBps).unwrap_or(5)
    }

    pub fn set_debt_ceiling(env: Env, ceiling: i128) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        if ceiling <= 0 {
            return Err(LendingError::Overflow);
        }
        env.storage().instance().set(&DataKey::DebtCeiling, &ceiling);
        Ok(())
    }

    pub fn deposit(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_emergency_status(&env, ProtocolAction::Deposit);
        if amount <= 0 {
            return Err(LendingError::InvalidAmount);
        }

        let active: bool = env
            .storage()
            .instance()
            .get(&DataKey::FlashActive)
            .unwrap_or(false);
        if active {
            panic!("FlashLoanReentrancy");
        }
        user.require_auth();

        let total_deposits: i128 = env
            .storage().persistent().get(&DataKey::TotalDeposits).unwrap_or(0);
        let deposit_cap: i128 = env
            .storage().persistent().get(&DataKey::DepositCap).unwrap_or(DEFAULT_DEPOSIT_CAP);
        let new_total = total_deposits.checked_add(amount).ok_or(LendingError::Overflow)?;
        if new_total > deposit_cap {
            return Err(LendingError::DepositCapExceeded);
        }

        let key = DataKey::Collateral(user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_balance = current.checked_add(amount).ok_or(LendingError::Overflow)?;
        env.storage().persistent().set(&key, &new_balance);
        env.storage()
            .persistent()
            .set(&DataKey::TotalDeposits, &new_total);
        extend_collateral_ttl(&env, &user);
        Ok(new_balance)
    }

    pub fn withdraw(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_emergency_status(&env, ProtocolAction::Withdraw);
        if amount <= 0 {
            return Err(LendingError::InvalidAmount);
        }

        let active: bool = env
            .storage()
            .instance()
            .get(&DataKey::FlashActive)
            .unwrap_or(false);
        if active {
            panic!("FlashLoanReentrancy");
        }
        user.require_auth();
        let key = DataKey::Collateral(user.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if amount > current {
            return Err(LendingError::InsufficientCollateral);
        }
        let new_balance = current
            .checked_sub(amount)
            .ok_or(LendingError::Overflow)?;
        env.storage().persistent().set(&key, &new_balance);
        let total_deposits: i128 = env.storage().persistent().get(&DataKey::TotalDeposits).unwrap_or(0);
        let new_total = total_deposits.saturating_sub(amount);
        env.storage().persistent().set(&DataKey::TotalDeposits, &new_total);
        extend_collateral_ttl(&env, &user);
        Ok(new_balance)
    }

    pub fn borrow(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_emergency_status(&env, ProtocolAction::Borrow);
        if amount <= 0 {
            return Err(LendingError::InvalidAmount);
        }
        user.require_auth();
        let min_borrow = Self::get_min_borrow(env.clone());
        if amount < min_borrow {
            return Err(LendingError::BelowMinimumBorrow);
        }

        let now = env.ledger().timestamp();
        let position = load_debt(&env, &user);
        let prev_principal = position.principal;
        let rate = current_borrow_rate(&env);
        let updated =
            borrow_amount(position, now, amount, rate).map_err(|e| match e {
                debt::DebtError::InvalidAmount => LendingError::InvalidAmount,
                debt::DebtError::Overflow => LendingError::Overflow,
            })?;
        save_debt(&env, &user, &updated);
        let total_debt: i128 = env.storage().persistent().get(&DataKey::TotalDebt).unwrap_or(0);
        let delta = updated.principal.checked_sub(prev_principal).expect("borrow: delta overflow");
        let new_total_debt = total_debt.checked_add(delta).expect("borrow: total_debt overflow");
        env.storage().persistent().set(&DataKey::TotalDebt, &new_total_debt);
        Ok(updated.principal)
    }

    pub fn liquidate(
        env: Env,
        liquidator: Address,
        borrower: Address,
        amount: i128,
    ) -> Result<i128, LendingError> {
        liquidator.require_auth();
        let col_key = DataKey::Collateral(borrower.clone());
        let debt_key = DataKey::Debt(borrower.clone());
        let collateral: i128 = env.storage().persistent().get(&col_key).unwrap_or(0);
        let debt: i128 = env.storage().persistent().get(&debt_key).unwrap_or(0);
        if debt == 0 {
            return Err(LendingError::PositionHealthy);
        }

        const LIQUIDATION_THRESHOLD: i128 = 8_000;
        let hf = (collateral * LIQUIDATION_THRESHOLD) / debt;

        if hf >= 10000 {
            return Err(LendingError::PositionHealthy);
        }

        const CLOSE_FACTOR: i128 = 5_000;
        let max_repay = (debt * CLOSE_FACTOR) / BPS_DENOM;
        let actual_repay = amount.min(max_repay);

        const INCENTIVE_BPS: i128 = 1_000;
        let seized_collateral = (actual_repay * (BPS_DENOM + INCENTIVE_BPS)) / BPS_DENOM;
        let final_seized = seized_collateral.min(collateral);

        env.storage()
            .persistent()
            .set(&debt_key, &(debt - actual_repay));
        env.storage()
            .persistent()
            .set(&col_key, &(collateral - final_seized));

        Ok(actual_repay)
    }

    pub fn repay(env: Env, user: Address, amount: i128) -> Result<i128, LendingError> {
        check_pause_status(&env, ProtocolAction::Repay);
        check_emergency_status(&env, ProtocolAction::Repay);

        if amount <= 0 {
            return Err(LendingError::InvalidAmount);
        }

        let active: bool = env
            .storage()
            .instance()
            .get(&DataKey::FlashActive)
            .unwrap_or(false);
        if active {
            panic!("FlashLoanReentrancy");
        }
        user.require_auth();
        let now = env.ledger().timestamp();
        let position = load_debt(&env, &user);
        let prev_principal = position.principal;
        let rate = current_borrow_rate(&env);
        let updated =
            repay_amount(position, now, amount, rate).map_err(|e| match e {
                debt::DebtError::InvalidAmount => LendingError::InvalidAmount,
                debt::DebtError::Overflow => LendingError::Overflow,
            })?;
        save_debt(&env, &user, &updated);
        let total_debt: i128 = env.storage().persistent().get(&DataKey::TotalDebt).unwrap_or(0);
        let repaid = prev_principal.checked_sub(updated.principal).unwrap_or(0);
        let new_total_debt = total_debt.saturating_sub(repaid);
        env.storage().persistent().set(&DataKey::TotalDebt, &new_total_debt);
        extend_debt_ttl(&env, &user);
        Ok(updated.principal)
    }

    pub fn get_debt_position(env: Env, user: Address) -> DebtPosition {
        let position = load_debt(&env, &user);
        if position.principal != 0 {
            extend_debt_ttl(&env, &user);
        }
        position
    }

    pub fn repay_flash_loan(env: Env, payer: Address, asset: Address, amount: i128) {
        payer.require_auth();
        let payer_key = DataKey::Balance(asset.clone(), payer.clone());
        let payer_bal: i128 = env.storage().persistent().get(&payer_key).unwrap_or(0);
        if payer_bal < amount {
            panic!("InsufficientBalance");
        }
        let new_payer_bal = payer_bal
            .checked_sub(amount)
            .expect("repay_flash_loan: payer balance underflow");
        env.storage().persistent().set(&payer_key, &new_payer_bal);

        let tre_key = DataKey::Treasury(asset.clone());
        let tre_bal: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        let new_tre_bal = tre_bal
            .checked_add(amount)
            .expect("repay_flash_loan: treasury balance overflow");
        env.storage().persistent().set(&tre_key, &new_tre_bal);
    }

    pub fn flash_loan(
        env: Env,
        initiator: Address,
        receiver: Address,
        asset: Address,
        amount: i128,
        params: Bytes,
    ) {
        let tre_key = DataKey::Treasury(asset.clone());
        let tre_bal: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        if amount > tre_bal {
            panic!("InsufficientLiquidity");
        }

        initiator.require_auth();

        let fee_bps = Self::get_flash_fee_bps(&env);
        let fee = amount
            .checked_mul(fee_bps)
            .map(|v| v / BPS_DENOM)
            .expect("flash_loan: fee calculation overflow");

        let new_tre_bal = tre_bal
            .checked_sub(amount)
            .expect("flash_loan: treasury underflow during transfer");
        env.storage().persistent().set(&tre_key, &new_tre_bal);

        let rec_key = DataKey::Balance(asset.clone(), receiver.clone());
        let rec_bal: i128 = env.storage().persistent().get(&rec_key).unwrap_or(0);
        let new_rec_bal = rec_bal
            .checked_add(amount)
            .expect("flash_loan: receiver balance overflow");
        env.storage().persistent().set(&rec_key, &new_rec_bal);

        env.storage().instance().set(&DataKey::FlashActive, &true);

        let method = Symbol::new(&env, "on_flash_loan");
        env.invoke_contract::<Val>(
            &receiver,
            &method,
            soroban_sdk::vec![
                &env,
                initiator.into_val(&env),
                asset.into_val(&env),
                amount.into_val(&env),
                fee.into_val(&env),
                params.into_val(&env)
            ],
        );

        env.storage().instance().set(&DataKey::FlashActive, &false);

        let final_tre: i128 = env.storage().persistent().get(&tre_key).unwrap_or(0);
        let required_balance = tre_bal
            .checked_add(fee)
            .expect("flash_loan: fee addition overflow");
        if final_tre < required_balance {
            panic!("InsufficientRepayment");
        }
    }

    pub fn get_position(env: Env, user: Address) -> PositionSummary {
        let col_key = DataKey::Collateral(user.clone());
        let col: i128 = env.storage().persistent().get(&col_key).unwrap_or(0);
        if col != 0 {
            extend_collateral_ttl(&env, &user);
        }
        let position = load_debt(&env, &user);
        if position.principal != 0 {
            extend_debt_ttl(&env, &user);
        }
        let rate = current_borrow_rate(&env);
        let debt = effective_debt(&position, env.ledger().timestamp(), rate)
            .unwrap_or(position.principal);
        let health_factor = if debt > 0 {
            col.checked_mul(8_000)
                .map(|v| v / debt)
                .unwrap_or(i128::MAX)
        } else {
            1_000_000
        };

        PositionSummary {
            collateral: col,
            debt,
            health_factor,
        }
    }

    pub fn get_protocol_metrics(env: Env) -> ProtocolMetrics {
        let total_supply: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalDeposits)
            .unwrap_or(0);
        let total_borrow: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalDebt)
            .unwrap_or(0);
        let utilization_bps = if total_supply > 0 {
            total_borrow.saturating_mul(10_000) / total_supply
        } else {
            0
        };
        ProtocolMetrics {
            total_supply,
            total_borrow,
            utilization_bps,
            ledger: env.ledger().sequence(),
        }
    }

    pub fn set_rate_params(env: Env, params: rate_model::RateParams) -> Result<(), LendingError> {
        let admin = Self::get_admin(env.clone());
        admin.require_auth();
        env.storage().instance().set(&DataKey::RateParams, &params);
        Ok(())
    }

    pub fn get_rate_params(env: Env) -> rate_model::RateParams {
        env.storage()
            .instance()
            .get(&DataKey::RateParams)
            .unwrap_or_default()
    }

    pub fn get_borrow_rate(env: Env) -> i128 {
        current_borrow_rate(&env)
    }
}

fn extend_collateral_ttl(env: &Env, user: &Address) {
    let key = DataKey::Collateral(user.clone());
    let extend_to = env.storage().max_ttl().min(PERSISTENT_TTL_LEDGERS);
    let threshold = extend_to / 2 + 1;
    if env.storage().persistent().has(&key) {
        env.storage()
            .persistent()
            .extend_ttl(&key, threshold, extend_to);
    }
}

fn extend_debt_ttl(env: &Env, user: &Address) {
    let key = DataKey::Debt(user.clone());
    let extend_to = env.storage().max_ttl().min(PERSISTENT_TTL_LEDGERS);
    let threshold = extend_to / 2 + 1;
    if env.storage().persistent().has(&key) {
        env.storage()
            .persistent()
            .extend_ttl(&key, threshold, extend_to);
    }
}

fn pause_is_active(env: &Env, pause_type: PauseType) -> bool {
    let key = DataKey::PauseState(pause_type);
    match env.storage().instance().get::<DataKey, PauseState>(&key) {
        Some(state) => state.paused && env.ledger().sequence() <= state.expires_at_ledger,
        None => false,
    }
}

fn check_pause_status(env: &Env, action: ProtocolAction) {
    if pause_is_active(env, PauseType::All) {
        panic!("OperationPaused");
    }
    let operation = match action {
        ProtocolAction::Deposit => PauseType::Deposit,
        ProtocolAction::Withdraw => PauseType::Withdraw,
        ProtocolAction::Borrow => PauseType::Borrow,
        ProtocolAction::Repay => PauseType::Repay,
        ProtocolAction::Liquidate => PauseType::Liquidation,
    };
    if pause_is_active(env, operation) {
        panic!("OperationPaused");
    }
}

fn set_emergency_state_internal(env: &Env, state: EmergencyState) {
    env.storage().instance().set(&DataKey::EmergencyState, &state);
}

fn get_emergency_state(env: &Env) -> EmergencyState {
    env.storage()
        .instance()
        .get(&DataKey::EmergencyState)
        .unwrap_or(EmergencyState::Normal)
}

fn check_emergency_status(env: &Env, action: ProtocolAction) {
    match get_emergency_state(env) {
        EmergencyState::Normal => {}
        EmergencyState::Shutdown => panic!("OperationDisabledDuringShutdown"),
        EmergencyState::Recovery => match action {
            ProtocolAction::Repay | ProtocolAction::Withdraw => {}
            _ => panic!("ActionBlockedInRecovery"),
        },
    }
}

fn current_borrow_rate(env: &Env) -> i128 {
    let params = env
        .storage()
        .instance()
        .get::<DataKey, rate_model::RateParams>(&DataKey::RateParams);

    match params {
        Some(p) => {
            let total_debt: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::TotalDebt)
                .unwrap_or(0);
            let total_supply: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::TotalDeposits)
                .unwrap_or(0);
            let utilization_bps = if total_supply > 0 {
                total_debt.saturating_mul(10_000) / total_supply
            } else {
                0
            };
            rate_model::compute_borrow_rate(utilization_bps, &p)
        }
        None => DEFAULT_APR_BPS,
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::LedgerInfo;

    fn setup() -> (
        Env,
        LendingContractClient<'static>,
        soroban_sdk::Address,
        soroban_sdk::Address,
    ) {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &id);
        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        client.initialize(&admin);
        (env, client, admin, user)
    }

    fn advance_time(env: &Env, seconds: u64) {
        let mut li: soroban_sdk::testutils::LedgerInfo = env.ledger().get();
        li.timestamp = li.timestamp.saturating_add(seconds);
        li.sequence_number = li.sequence_number.saturating_add(seconds as u32);
        env.ledger().set(li);
    }

    #[test]
    fn test_initialize_and_get_admin() {
        let (_env, client, admin, _user) = setup();
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    fn test_set_min_borrow_admin_only() {
        let (_env, client, _admin, _user) = setup();
        assert_eq!(client.get_min_borrow(), 0);
        client.set_min_borrow(&100);
        assert_eq!(client.get_min_borrow(), 100);
    }

    #[test]
    fn test_set_debt_ceiling_admin_only() {
        let (_env, client, _admin, _user) = setup();
        client.set_debt_ceiling(&1_000_000).unwrap();
    }

    #[test]
    fn test_set_flash_fee_valid_range() {
        let (_env, client, _admin, _user) = setup();
        client.set_flash_fee(&50).unwrap();
    }

    #[test]
    fn test_set_flash_fee_rejects_out_of_range() {
        let (_env, client, _admin, _user) = setup();
        let res = client.try_set_flash_fee(&1_001);
        assert!(
            matches!(res, Err(Ok(LendingError::InvalidFeeBps))),
            "expected InvalidFeeBps, got {:?}",
            res
        );
    }

    #[test]
    fn test_deposit_increases_balance() {
        let (_env, client, _admin, user) = setup();
        assert_eq!(client.deposit(&user, &100), 100);
        assert_eq!(client.deposit(&user, &50), 150);
    }

    #[test]
    fn test_withdraw_decreases_balance() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &100);
        assert_eq!(client.withdraw(&user, &40), 60);
    }

    #[test]
    fn test_withdraw_fails_when_over_withdrawing() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &50);
        assert!(client.try_withdraw(&user, &75).is_err());
    }

    #[test]
    fn test_borrow_increases_debt() {
        let (_env, client, _admin, user) = setup();
        assert_eq!(client.borrow(&user, &50), 50);
    }

    #[test]
    fn test_repay_decreases_debt() {
        let (_env, client, _admin, user) = setup();
        client.borrow(&user, &100);
        assert_eq!(client.repay(&user, &30), 70);
    }

    #[test]
    fn test_position_summary_reflects_state() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &200);
        client.borrow(&user, &75);
        let pos = client.get_position(&user);
        assert_eq!(pos.collateral, 200);
        assert_eq!(pos.debt, 75);
    }

    #[test]
    fn test_position_summary_default_zero() {
        let (_env, client, _admin, user) = setup();
        let pos = client.get_position(&user);
        assert_eq!(pos.collateral, 0);
        assert_eq!(pos.debt, 0);
    }

    #[test]
    fn test_borrow_below_minimum_rejected() {
        let (_env, client, _admin, user) = setup();
        client.set_min_borrow(&50).unwrap();
        assert!(client.try_borrow(&user, &40).is_err());
    }

    #[test]
    fn test_borrow_exactly_minimum_accepted() {
        let (_env, client, _admin, user) = setup();
        client.set_min_borrow(&50).unwrap();
        assert_eq!(client.borrow(&user, &50), 50);
    }

    #[test]
    fn test_ttl_keeps_position_live_across_reads() {
        let (env, client, _admin, user) = setup();
        client.deposit(&user, &200);
        client.borrow(&user, &75);

        advance_time(&env, (PERSISTENT_TTL_LEDGERS / 2) as u64);
        let pos_mid = client.get_position(&user);
        assert_eq!(pos_mid.collateral, 200);

        advance_time(&env, (PERSISTENT_TTL_LEDGERS / 2 + 1) as u64);
        let pos_after = client.get_position(&user);
        assert_eq!(pos_after.collateral, 200);
    }

    #[test]
    fn test_get_debt_position_extends_debt_ttl() {
        let (env, client, _admin, user) = setup();
        client.borrow(&user, &100);
        advance_time(&env, (PERSISTENT_TTL_LEDGERS / 2) as u64);
        assert_eq!(client.get_debt_position(&user).principal, 100);
        advance_time(&env, (PERSISTENT_TTL_LEDGERS / 2 + 1) as u64);
        assert_eq!(client.get_debt_position(&user).principal, 100);
    }

    #[test]
    fn test_guardian_can_shutdown() {
        let (env, client, _admin, _user) = setup();
        let guardian = Address::generate(&env);
        client.set_guardian(&guardian);
        client.set_emergency_state(&EmergencyState::Shutdown);
        let user = Address::generate(&env);
        let result = client.try_deposit(&user, &10);
        assert!(result.is_err());
    }

    #[test]
    #[should_panic]
    fn test_non_guardian_cannot_set_state() {
        let (env2, _, _, _) = setup();
        let env2 = Env::default();
        let id2 = env2.register(LendingContract, ());
        let client2 = LendingContractClient::new(&env2, &id2);
        let admin2 = Address::generate(&env2);
        let attacker = Address::generate(&env2);
        env2.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &admin2,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &id2,
                fn_name: "initialize",
                args: (admin2.clone(),).into_val(&env2),
                sub_invokes: &[],
            },
        }]);
        client2.initialize(&admin2);
        client2.set_emergency_state(&EmergencyState::Shutdown);
    }

    #[test]
    fn test_admin_can_set_recovery() {
        let (_env, client, _admin, _user) = setup();
        client.set_emergency_state(&EmergencyState::Recovery);
    }

    #[test]
    fn test_admin_can_set_normal() {
        let (_env, client, _admin, _user) = setup();
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.set_emergency_state(&EmergencyState::Normal);
    }

    #[test]
    fn test_admin_lifts_shutdown_to_normal() {
        let (env, client, _admin, _user) = setup();
        let guardian = Address::generate(&env);
        client.set_guardian(&guardian);
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.set_emergency_state(&EmergencyState::Normal);
        let user = Address::generate(&env);
        let result = client.deposit(&user, &10);
        assert_eq!(result, 10);
    }

    #[test]
    #[should_panic(expected = "Unauthorized")]
    fn test_random_caller_cannot_set_emergency_state() {
        let env2 = Env::default();
        let id2 = env2.register(LendingContract, ());
        let client2 = LendingContractClient::new(&env2, &id2);
        let admin2 = Address::generate(&env2);
        let attacker = Address::generate(&env2);
        env2.mock_all_auths();
        client2.initialize(&admin2);
        env2.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &attacker,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &id2,
                fn_name: "set_emergency_state",
                args: (EmergencyState::Shutdown,).into_val(&env2),
                sub_invokes: &[],
            },
        }]);
        client2.set_emergency_state(&EmergencyState::Shutdown);
    }

    #[test]
    #[should_panic(expected = "OperationDisabledDuringShutdown")]
    fn test_shutdown_blocks_deposit() {
        let (_env, client, _admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.deposit(&user, &10);
    }

    #[test]
    #[should_panic(expected = "OperationDisabledDuringShutdown")]
    fn test_shutdown_blocks_borrow() {
        let (_env, client, _admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.borrow(&user, &5);
    }

    #[test]
    #[should_panic(expected = "OperationDisabledDuringShutdown")]
    fn test_shutdown_blocks_repay() {
        let (_env, client, _admin, user) = setup();
        client.borrow(&user, &100);
        client.set_emergency_state(&EmergencyState::Shutdown);
        client.repay(&user, &10);
    }

    #[test]
    #[should_panic(expected = "ActionBlockedInRecovery")]
    fn test_recovery_blocks_deposit() {
        let (_env, client, _admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Recovery);
        client.deposit(&user, &10);
    }

    #[test]
    #[should_panic(expected = "ActionBlockedInRecovery")]
    fn test_recovery_blocks_borrow() {
        let (_env, client, _admin, user) = setup();
        client.set_emergency_state(&EmergencyState::Recovery);
        client.borrow(&user, &10);
    }

    #[test]
    fn test_recovery_allows_repay_and_withdraw() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &200);
        client.borrow(&user, &50);
        client.set_emergency_state(&EmergencyState::Recovery);
        assert_eq!(client.repay(&user, &10), 40);
        assert_eq!(client.withdraw(&user, &10), 190);
    }

    #[test]
    fn test_protocol_metrics_initial_zeros() {
        let (_env, client, _admin, _user) = setup();
        let m = client.get_protocol_metrics();
        assert_eq!(m.total_supply, 0);
        assert_eq!(m.total_borrow, 0);
        assert_eq!(m.utilization_bps, 0);
    }

    #[test]
    fn test_protocol_metrics_after_deposit() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &1000);
        let m = client.get_protocol_metrics();
        assert_eq!(m.total_supply, 1000);
        assert_eq!(m.total_borrow, 0);
        assert_eq!(m.utilization_bps, 0);
    }

    #[test]
    fn test_protocol_metrics_after_borrow() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &1000);
        client.borrow(&user, &500);
        let m = client.get_protocol_metrics();
        assert_eq!(m.total_supply, 1000);
        assert_eq!(m.total_borrow, 500);
        assert_eq!(m.utilization_bps, 5000);
    }

    #[test]
    fn test_protocol_metrics_after_repay() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &1000);
        client.borrow(&user, &500);
        client.repay(&user, &200);
        let m = client.get_protocol_metrics();
        assert_eq!(m.total_supply, 1000);
        assert_eq!(m.total_borrow, 300);
        assert_eq!(m.utilization_bps, 3000);
    }

    #[test]
    fn test_protocol_metrics_interleaved_multiple_users() {
        let (env, client, _admin, user1) = setup();
        let user2 = Address::generate(&env);
        client.deposit(&user1, &600);
        client.deposit(&user2, &400);
        client.borrow(&user1, &200);
        client.borrow(&user2, &100);
        let m = client.get_protocol_metrics();
        assert_eq!(m.total_supply, 1000);
        assert_eq!(m.total_borrow, 300);
        assert_eq!(m.utilization_bps, 3000);
    }

    #[test]
    fn test_protocol_metrics_full_repay_zeroes_borrow() {
        let (_env, client, _admin, user) = setup();
        client.deposit(&user, &1000);
        client.borrow(&user, &400);
        client.repay(&user, &400);
        let m = client.get_protocol_metrics();
        assert_eq!(m.total_borrow, 0);
        assert_eq!(m.utilization_bps, 0);
    }

    #[test]
    fn test_protocol_metrics_ledger_field_set() {
        let (env, client, _admin, _user) = setup();
        let m = client.get_protocol_metrics();
        assert_eq!(m.ledger, env.ledger().sequence());
    }

    #[test]
    fn test_get_rate_params_defaults() {
        let (_env, client, _admin, _user) = setup();
        let p = client.get_rate_params();
        assert_eq!(p.base_rate_bps, 100);
        assert_eq!(p.kink_utilization_bps, 8_000);
        assert_eq!(p.multiplier_bps, 2_000);
        assert_eq!(p.jump_multiplier_bps, 10_000);
        assert_eq!(p.rate_floor_bps, 50);
        assert_eq!(p.rate_ceiling_bps, 10_000);
    }

    #[test]
    fn test_set_rate_params_admin_only() {
        let (_env, client, _admin, _user) = setup();
        let p = rate_model::RateParams {
            base_rate_bps: 200,
            kink_utilization_bps: 7_500,
            multiplier_bps: 1_500,
            jump_multiplier_bps: 8_000,
            rate_floor_bps: 100,
            rate_ceiling_bps: 8_000,
        };
        client.set_rate_params(&p).unwrap();
        let stored = client.get_rate_params();
        assert_eq!(stored, p);
    }

    #[test]
    fn test_get_borrow_rate_uses_default_when_no_params_set() {
        let (_env, client, _admin, _user) = setup();
        let rate = client.get_borrow_rate();
        assert_eq!(rate, DEFAULT_APR_BPS);
    }

    #[test]
    fn test_get_borrow_rate_reflects_utilization() {
        let (_env, client, _admin, user) = setup();
        let p = rate_model::RateParams {
            base_rate_bps: 0,
            kink_utilization_bps: 10_000,
            multiplier_bps: 2_000,
            jump_multiplier_bps: 5_000,
            rate_floor_bps: 0,
            rate_ceiling_bps: 10_000,
        };
        client.set_rate_params(&p).unwrap();
        client.deposit(&user, &1_000);
        client.borrow(&user, &500);
        let rate = client.get_borrow_rate();
        assert_eq!(rate, 1_000);
    }

    #[test]
    fn test_get_borrow_rate_zero_supply_returns_base_rate() {
        let (_env, client, _admin, _user) = setup();
        let p = rate_model::RateParams {
            base_rate_bps: 150,
            ..Default::default()
        };
        client.set_rate_params(&p).unwrap();
        let rate = client.get_borrow_rate();
        assert_eq!(rate, 150);
    }

    #[test]
    fn test_borrow_and_repay_use_dynamic_rate() {
        let (env, client, _admin, user) = setup();
        let p = rate_model::RateParams {
            base_rate_bps: 0,
            kink_utilization_bps: 10_000,
            multiplier_bps: 10_000,
            jump_multiplier_bps: 10_000,
            rate_floor_bps: 0,
            rate_ceiling_bps: 10_000,
        };
        client.set_rate_params(&p).unwrap();
        client.deposit(&user, &10_000);
        client.borrow(&user, &5_000);
        advance_time(&env, 3600);
        let pos = client.get_position(&user);
        assert!(pos.debt > 5_000, "debt should have accrued, got {}", pos.debt);
    }

    #[test]
    #[should_panic(expected = "Unauthorized")]
    fn test_rate_params_reject_non_admin() {
        let env = Env::default();
        let id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &id);
        let admin = Address::generate(&env);
        let attacker = Address::generate(&env);
        env.mock_all_auths();
        client.initialize(&admin);
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &attacker,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &id,
                fn_name: "set_rate_params",
                args: (rate_model::RateParams::default(),).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.set_rate_params(&rate_model::RateParams::default());
    }
}
