#![no_std]
use emergency_guard::{EmergencyGuard, GuardError, PauseType};
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, vec, Address, Env,
    String, Vec,
};

#[cfg(test)]
mod fuzz_test;
#[cfg(test)]
mod test;

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors returned by the `LiquidityPool` contract.
///
/// Code assignments are stable on-chain ABI — do NOT renumber.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InsufficientBalance = 4,
    InsufficientLiquidity = 5,
    InsufficientShares = 6,
    InsufficientAllowance = 7,
    SlippageExceeded = 8,
    InvalidFee = 9,
    OracleNotConfigured = 10,
    PendingFeeUpdateExists = 11,
    TimelockNotElapsed = 12,
    NoPendingFeeUpdate = 13,
    Paused = 14,
}

// ── Event payloads ────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepositEvent {
    pub user: Address,
    pub amount_a: i128,
    pub amount_b: i128,
    pub shares_minted: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwapEvent {
    pub user: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: i128,
    pub amount_out: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WithdrawEvent {
    pub user: Address,
    pub shares_burned: i128,
    pub amount_a: i128,
    pub amount_b: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BurnEvent {
    pub user: Address,
    pub shares_burned: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeeChangedEvent {
    pub admin: Address,
    pub old_fee_bps: i128,
    pub new_fee_bps: i128,
}

// ── Fee constants ─────────────────────────────────────────────────────────────

pub const MAX_FEE_BPS: i128 = 100;
pub const DEFAULT_BASE_FEE_BPS: i128 = 30;
pub const DEFAULT_FEE_TIMELOCK_LEDGERS: u32 = 120;

pub const LOW_VOLATILITY_THRESHOLD_BPS: i128 = 100;
pub const MEDIUM_VOLATILITY_THRESHOLD_BPS: i128 = 250;
pub const HIGH_VOLATILITY_THRESHOLD_BPS: i128 = 500;

pub const LOW_VOLATILITY_FEE_BPS: i128 = 40;
pub const MEDIUM_VOLATILITY_FEE_BPS: i128 = 70;
pub const HIGH_VOLATILITY_FEE_BPS: i128 = 100;

// ── Oracle interface ──────────────────────────────────────────────────────────

#[soroban_sdk::contractclient(name = "PriceOracleClient")]
pub trait PriceOracle {
    fn latest_price(e: Env) -> i128;
}

// ── On-chain data types ───────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoolState {
    pub token_a: Address,
    pub token_b: Address,
    pub reserve_a: i128,
    pub reserve_b: i128,
    pub total_shares: i128,
    pub fee_bps: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingFeeUpdate {
    pub new_fee_bps: i128,
    pub executable_after_ledger: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OracleConfig {
    pub oracle_id: Address,
    pub base_fee_bps: i128,
    pub timelock_ledgers: u32,
}

// ── Per-user storage keys (persistent, keyed by address) ─────────────────────

#[derive(Clone)]
#[contracttype]
pub struct AllowanceDataKey {
    pub from: Address,
    pub spender: Address,
}

#[derive(Clone)]
#[contracttype]
pub struct AllowanceValue {
    pub amount: i128,
    pub expiration_ledger: u32,
}

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Pool,
    Admin,
    Balance(Address),
    Allowance(AllowanceDataKey),
    OracleConfig,
    LastOraclePrice,
    LastVolatilityBps,
    PendingFeeUpdate,
}

// ── Private helpers ───────────────────────────────────────────────────────────

fn sqrt(x: i128) -> i128 {
    if x == 0 {
        return 0;
    }
    let mut z = (x + 1) / 2;
    let mut y = x;
    while z < y {
        y = z;
        z = (x / z + z) / 2;
    }
    y
}

fn load_pool(e: &Env) -> Result<PoolState, Error> {
    e.storage()
        .instance()
        .get(&DataKey::Pool)
        .ok_or(Error::NotInitialized)
}

fn save_pool(e: &Env, pool: &PoolState) {
    e.storage().instance().set(&DataKey::Pool, pool);
}

fn load_admin(e: &Env) -> Result<Address, Error> {
    e.storage()
        .instance()
        .get(&DataKey::Admin)
        .ok_or(Error::NotInitialized)
}

fn map_guard_err(err: GuardError) -> Error {
    match err {
        GuardError::Paused => Error::Paused,
        GuardError::NotInitialized => Error::NotInitialized,
        GuardError::Unauthorized
        | GuardError::InsufficientSignatures
        | GuardError::AdminNotFound
        | GuardError::InvalidThreshold => Error::Unauthorized,
        GuardError::AlreadyInitialized => Error::AlreadyInitialized,
    }
}

fn require_not_paused(e: &Env, operation: u32) -> Result<(), Error> {
    if EmergencyGuard::is_paused(e.clone(), operation) {
        Err(Error::Paused)
    } else {
        Ok(())
    }
}

fn target_fee_from_volatility(base_fee_bps: i128, volatility_bps: i128) -> i128 {
    let dynamic = if volatility_bps >= HIGH_VOLATILITY_THRESHOLD_BPS {
        HIGH_VOLATILITY_FEE_BPS
    } else if volatility_bps >= MEDIUM_VOLATILITY_THRESHOLD_BPS {
        MEDIUM_VOLATILITY_FEE_BPS
    } else if volatility_bps >= LOW_VOLATILITY_THRESHOLD_BPS {
        LOW_VOLATILITY_FEE_BPS
    } else {
        base_fee_bps
    };
    if dynamic > MAX_FEE_BPS {
        MAX_FEE_BPS
    } else {
        dynamic
    }
}

const CORE_PAUSE_OPS: [u32; 4] = [
    PauseType::SWAP,
    PauseType::DEPOSIT,
    PauseType::WITHDRAW,
    PauseType::BURN,
];

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct LiquidityPool;

#[contractimpl]
impl LiquidityPool {
    // ── Initialisation ────────────────────────────────────────────────────────

    pub fn initialize(
        e: Env,
        admin: Address,
        token_a: Address,
        token_b: Address,
    ) -> Result<(), Error> {
        if e.storage().instance().has(&DataKey::Pool) {
            return Err(Error::AlreadyInitialized);
        }

        e.storage().instance().set(&DataKey::Admin, &admin);
        save_pool(
            &e,
            &PoolState {
                token_a,
                token_b,
                reserve_a: 0,
                reserve_b: 0,
                total_shares: 0,
                fee_bps: DEFAULT_BASE_FEE_BPS,
            },
        );

        EmergencyGuard::initialize(e.clone(), vec![&e, admin], 1).map_err(map_guard_err)?;
        Ok(())
    }

    // ── Admin accessors ─────────────────────────────────────────────────────────

    pub fn get_admin(e: Env) -> Address {
        load_admin(&e).expect("not initialized")
    }

    pub fn get_admins(e: Env) -> Vec<Address> {
        EmergencyGuard::get_admins(e)
    }

    pub fn get_admin_threshold(e: Env) -> u32 {
        EmergencyGuard::get_threshold(e)
    }

    // ── EmergencyGuard pause interface ──────────────────────────────────────────

    pub fn guard_pause(
        e: Env,
        admin: Address,
        operation: u32,
        paused: bool,
    ) -> Result<(), Error> {
        EmergencyGuard::set_pause(e, admin, operation, paused).map_err(map_guard_err)
    }

    pub fn guard_is_paused(e: Env, operation: u32) -> bool {
        EmergencyGuard::is_paused(e, operation)
    }

    pub fn set_operation_paused(
        e: Env,
        admin: Address,
        operation: u32,
        paused: bool,
    ) -> Result<(), Error> {
        Self::guard_pause(e, admin, operation, paused)
    }

    /// Pause or unpause all core pool operations (swap, deposit, withdraw, burn).
    pub fn set_paused(e: Env, paused: bool) -> Result<(), Error> {
        let admin = load_admin(&e)?;
        for op in CORE_PAUSE_OPS {
            EmergencyGuard::set_pause(e.clone(), admin.clone(), op, paused).map_err(map_guard_err)?;
        }
        Ok(())
    }

    pub fn pause_swaps(e: Env) -> Result<(), Error> {
        let admin = load_admin(&e)?;
        EmergencyGuard::set_pause(e, admin, PauseType::SWAP, true).map_err(map_guard_err)
    }

    pub fn resume_swaps(e: Env) -> Result<(), Error> {
        let admin = load_admin(&e)?;
        EmergencyGuard::set_pause(e, admin, PauseType::SWAP, false).map_err(map_guard_err)
    }

    pub fn pause_deposits(e: Env) -> Result<(), Error> {
        let admin = load_admin(&e)?;
        EmergencyGuard::set_pause(e, admin, PauseType::DEPOSIT, true).map_err(map_guard_err)
    }

    pub fn resume_deposits(e: Env) -> Result<(), Error> {
        let admin = load_admin(&e)?;
        EmergencyGuard::set_pause(e, admin, PauseType::DEPOSIT, false).map_err(map_guard_err)
    }

    pub fn pause_withdrawals(e: Env) -> Result<(), Error> {
        let admin = load_admin(&e)?;
        EmergencyGuard::set_pause(e, admin, PauseType::WITHDRAW, true).map_err(map_guard_err)
    }

    pub fn resume_withdrawals(e: Env) -> Result<(), Error> {
        let admin = load_admin(&e)?;
        EmergencyGuard::set_pause(e, admin, PauseType::WITHDRAW, false).map_err(map_guard_err)
    }

    pub fn emergency_pause(e: Env, approvers: Vec<Address>) -> Result<(), Error> {
        EmergencyGuard::emergency_pause(e, approvers).map_err(map_guard_err)
    }

    pub fn resume(e: Env, approvers: Vec<Address>) -> Result<(), Error> {
        EmergencyGuard::resume(e, approvers).map_err(map_guard_err)
    }

    pub fn emergency_pause_all(e: Env, approvers: Vec<Address>) -> Result<(), Error> {
        Self::emergency_pause(e, approvers)
    }

    pub fn resume_all(e: Env, approvers: Vec<Address>) -> Result<(), Error> {
        Self::resume(e, approvers)
    }

    pub fn get_pause_state(e: Env) -> u32 {
        EmergencyGuard::get_pause_state(e)
    }

    pub fn is_paused_op(e: Env, operation: u32) -> bool {
        EmergencyGuard::is_paused(e, operation)
    }

    pub fn add_admin(e: Env, approvers: Vec<Address>, new_admin: Address) -> Result<(), Error> {
        EmergencyGuard::add_admin(e, approvers, new_admin).map_err(map_guard_err)
    }

    pub fn remove_admin(e: Env, approvers: Vec<Address>, admin: Address) -> Result<(), Error> {
        let pool_admin = load_admin(&e)?;
        EmergencyGuard::remove_admin(e.clone(), approvers, admin.clone()).map_err(map_guard_err)?;
        let admins = EmergencyGuard::get_admins(e.clone());
        if pool_admin == admin && admins.len() == 1 {
            if let Some(remaining) = admins.get(0) {
                e.storage().instance().set(&DataKey::Admin, &remaining);
            }
        }
        Ok(())
    }

    pub fn rotate_admin(
        e: Env,
        approvers: Vec<Address>,
        old_admin: Address,
        new_admin: Address,
    ) -> Result<(), Error> {
        let pool_admin = load_admin(&e)?;
        if pool_admin != old_admin {
            return Err(Error::Unauthorized);
        }
        EmergencyGuard::add_admin(e.clone(), approvers.clone(), new_admin.clone())
            .map_err(map_guard_err)?;
        EmergencyGuard::remove_admin(e.clone(), approvers, old_admin).map_err(map_guard_err)?;
        e.storage().instance().set(&DataKey::Admin, &new_admin);
        Ok(())
    }

    // ── Fee management ────────────────────────────────────────────────────────

    pub fn get_fee(e: Env) -> i128 {
        load_pool(&e)
            .map(|p| p.fee_bps)
            .unwrap_or(DEFAULT_BASE_FEE_BPS)
    }

    pub fn set_fee(e: Env, fee_bps: i128) -> Result<(), Error> {
        if !(0..=MAX_FEE_BPS).contains(&fee_bps) {
            return Err(Error::InvalidFee);
        }
        let admin = load_admin(&e)?;
        admin.require_auth();
        let mut pool = load_pool(&e)?;
        let old_fee = pool.fee_bps;
        pool.fee_bps = fee_bps;
        save_pool(&e, &pool);
        e.events().publish(
            (String::from_str(&e, "fee_changed"), admin.clone()),
            FeeChangedEvent {
                admin,
                old_fee_bps: old_fee,
                new_fee_bps: fee_bps,
            },
        );
        Ok(())
    }

    pub fn configure_fee_oracle(
        e: Env,
        oracle_id: Address,
        base_fee_bps: i128,
        timelock_ledgers: u32,
    ) -> Result<(), Error> {
        if !(0..=MAX_FEE_BPS).contains(&base_fee_bps) {
            return Err(Error::InvalidFee);
        }
        let admin = load_admin(&e)?;
        admin.require_auth();
        e.storage().instance().set(
            &DataKey::OracleConfig,
            &OracleConfig {
                oracle_id,
                base_fee_bps,
                timelock_ledgers,
            },
        );
        Ok(())
    }

    pub fn get_last_volatility_bps(e: Env) -> i128 {
        e.storage()
            .instance()
            .get(&DataKey::LastVolatilityBps)
            .unwrap_or(0)
    }

    pub fn get_pending_fee_update(e: Env) -> Option<PendingFeeUpdate> {
        e.storage().instance().get(&DataKey::PendingFeeUpdate)
    }

    pub fn sync_fee_from_oracle(e: Env) -> Result<Option<PendingFeeUpdate>, Error> {
        let cfg: OracleConfig = e
            .storage()
            .instance()
            .get(&DataKey::OracleConfig)
            .ok_or(Error::OracleNotConfigured)?;

        let oracle = PriceOracleClient::new(&e, &cfg.oracle_id);
        let current_price = oracle.latest_price();

        let maybe_last: Option<i128> = e.storage().instance().get(&DataKey::LastOraclePrice);
        let Some(last_price) = maybe_last else {
            e.storage()
                .instance()
                .set(&DataKey::LastOraclePrice, &current_price);
            return Ok(None);
        };

        let delta = if current_price >= last_price {
            current_price - last_price
        } else {
            last_price - current_price
        };
        let volatility_bps = if last_price == 0 {
            0
        } else {
            delta * 10_000 / last_price
        };

        e.storage()
            .instance()
            .set(&DataKey::LastOraclePrice, &current_price);
        e.storage()
            .instance()
            .set(&DataKey::LastVolatilityBps, &volatility_bps);

        let pool = load_pool(&e)?;
        let target_fee = target_fee_from_volatility(cfg.base_fee_bps, volatility_bps);

        if target_fee == pool.fee_bps {
            return Ok(None);
        }

        let executable_after_ledger = e.ledger().sequence() + cfg.timelock_ledgers;
        let pending = PendingFeeUpdate {
            new_fee_bps: target_fee,
            executable_after_ledger,
        };
        e.storage()
            .instance()
            .set(&DataKey::PendingFeeUpdate, &pending);
        Ok(Some(pending))
    }

    pub fn execute_fee_update(e: Env) -> Result<i128, Error> {
        let pending: PendingFeeUpdate = e
            .storage()
            .instance()
            .get(&DataKey::PendingFeeUpdate)
            .ok_or(Error::NoPendingFeeUpdate)?;

        if e.ledger().sequence() < pending.executable_after_ledger {
            return Err(Error::TimelockNotElapsed);
        }
        if !(0..=MAX_FEE_BPS).contains(&pending.new_fee_bps) {
            return Err(Error::InvalidFee);
        }

        let mut pool = load_pool(&e)?;
        pool.fee_bps = pending.new_fee_bps;
        save_pool(&e, &pool);
        e.storage().instance().remove(&DataKey::PendingFeeUpdate);
        Ok(pending.new_fee_bps)
    }

    // ── Core AMM operations ───────────────────────────────────────────────────

    pub fn deposit(e: Env, to: Address, amount_a: i128, amount_b: i128) -> Result<i128, Error> {
        require_not_paused(&e, PauseType::DEPOSIT)?;
        to.require_auth();

        let mut pool = load_pool(&e)?;

        let client_a = soroban_sdk::token::Client::new(&e, &pool.token_a);
        let client_b = soroban_sdk::token::Client::new(&e, &pool.token_b);
        client_a.transfer(&to, &e.current_contract_address(), &amount_a);
        client_b.transfer(&to, &e.current_contract_address(), &amount_b);

        let shares: i128 = if pool.total_shares == 0 {
            let product = amount_a
                .checked_mul(amount_b)
                .ok_or(Error::InsufficientLiquidity)?;
            sqrt(product)
        } else {
            let share_a = amount_a
                .checked_mul(pool.total_shares)
                .ok_or(Error::InsufficientLiquidity)?
                / pool.reserve_a;
            let share_b = amount_b
                .checked_mul(pool.total_shares)
                .ok_or(Error::InsufficientLiquidity)?
                / pool.reserve_b;
            if share_a < share_b {
                share_a
            } else {
                share_b
            }
        };

        let user_key = DataKey::Balance(to.clone());
        let current: i128 = e.storage().persistent().get(&user_key).unwrap_or(0);
        e.storage().persistent().set(&user_key, &(current + shares));
        e.storage().persistent().extend_ttl(&user_key, 100, 100);

        pool.total_shares += shares;
        pool.reserve_a += amount_a;
        pool.reserve_b += amount_b;
        save_pool(&e, &pool);

        e.events().publish(
            (String::from_str(&e, "deposit"), to.clone()),
            DepositEvent {
                user: to,
                amount_a,
                amount_b,
                shares_minted: shares,
            },
        );

        Ok(shares)
    }

    pub fn swap(e: Env, to: Address, buy_a: bool, out: i128, in_max: i128) -> Result<i128, Error> {
        require_not_paused(&e, PauseType::SWAP)?;
        to.require_auth();

        let mut pool = load_pool(&e)?;

        let (reserve_in, reserve_out, token_in, token_out) = if buy_a {
            (
                pool.reserve_b,
                pool.reserve_a,
                pool.token_b.clone(),
                pool.token_a.clone(),
            )
        } else {
            (
                pool.reserve_a,
                pool.reserve_b,
                pool.token_a.clone(),
                pool.token_b.clone(),
            )
        };

        if out >= reserve_out {
            return Err(Error::InsufficientLiquidity);
        }

        let fee_scale = 10_000i128 - pool.fee_bps;
        let numerator = reserve_in
            .checked_mul(out)
            .ok_or(Error::InsufficientLiquidity)?
            .checked_mul(10_000)
            .ok_or(Error::InsufficientLiquidity)?;
        let denominator = (reserve_out - out)
            .checked_mul(fee_scale)
            .ok_or(Error::InsufficientLiquidity)?;
        let amount_in = (numerator / denominator) + 1;

        if amount_in > in_max {
            return Err(Error::SlippageExceeded);
        }

        soroban_sdk::token::Client::new(&e, &token_in).transfer(
            &to,
            &e.current_contract_address(),
            &amount_in,
        );
        soroban_sdk::token::Client::new(&e, &token_out).transfer(
            &e.current_contract_address(),
            &to,
            &out,
        );

        if buy_a {
            pool.reserve_a -= out;
            pool.reserve_b += amount_in;
        } else {
            pool.reserve_a += amount_in;
            pool.reserve_b -= out;
        }
        save_pool(&e, &pool);

        e.events().publish(
            (String::from_str(&e, "swap"), to.clone()),
            SwapEvent {
                user: to,
                token_in,
                token_out,
                amount_in,
                amount_out: out,
            },
        );

        Ok(amount_in)
    }

    pub fn withdraw(e: Env, to: Address, share_amount: i128) -> Result<(i128, i128), Error> {
        require_not_paused(&e, PauseType::WITHDRAW)?;
        to.require_auth();

        let mut pool = load_pool(&e)?;

        let user_key = DataKey::Balance(to.clone());
        let current: i128 = e.storage().persistent().get(&user_key).unwrap_or(0);
        if share_amount > current {
            return Err(Error::InsufficientShares);
        }

        let amount_a = share_amount * pool.reserve_a / pool.total_shares;
        let amount_b = share_amount * pool.reserve_b / pool.total_shares;

        e.storage()
            .persistent()
            .set(&user_key, &(current - share_amount));
        e.storage().persistent().extend_ttl(&user_key, 100, 100);

        pool.total_shares -= share_amount;
        pool.reserve_a -= amount_a;
        pool.reserve_b -= amount_b;
        let token_a = pool.token_a.clone();
        let token_b = pool.token_b.clone();
        save_pool(&e, &pool);

        soroban_sdk::token::Client::new(&e, &token_a).transfer(
            &e.current_contract_address(),
            &to,
            &amount_a,
        );
        soroban_sdk::token::Client::new(&e, &token_b).transfer(
            &e.current_contract_address(),
            &to,
            &amount_b,
        );

        e.events().publish(
            (String::from_str(&e, "withdraw"), to.clone()),
            WithdrawEvent {
                user: to,
                shares_burned: share_amount,
                amount_a,
                amount_b,
            },
        );

        Ok((amount_a, amount_b))
    }

    pub fn burn(e: Env, from: Address, amount: i128) -> Result<(), Error> {
        require_not_paused(&e, PauseType::BURN)?;
        from.require_auth();

        let mut pool = load_pool(&e)?;

        let user_key = DataKey::Balance(from.clone());
        let current: i128 = e.storage().persistent().get(&user_key).unwrap_or(0);
        if amount > current {
            return Err(Error::InsufficientShares);
        }

        e.storage().persistent().set(&user_key, &(current - amount));
        e.storage().persistent().extend_ttl(&user_key, 100, 100);

        pool.total_shares -= amount;
        save_pool(&e, &pool);

        e.events().publish(
            (String::from_str(&e, "burn"), from.clone()),
            BurnEvent {
                user: from,
                shares_burned: amount,
            },
        );

        Ok(())
    }

    // ── ERC-20-style LP-share token interface ─────────────────────────────────

    pub fn name(e: Env) -> String {
        String::from_str(&e, "Liquidity Pool Share")
    }

    pub fn symbol(e: Env) -> String {
        String::from_str(&e, "LPS")
    }

    pub fn decimals(_e: Env) -> u32 {
        7
    }

    pub fn balance(e: Env, id: Address) -> i128 {
        e.storage()
            .persistent()
            .get(&DataKey::Balance(id))
            .unwrap_or(0)
    }

    pub fn total_supply(e: Env) -> i128 {
        load_pool(&e).map(|p| p.total_shares).unwrap_or(0)
    }

    pub fn transfer(e: Env, from: Address, to: Address, amount: i128) -> Result<(), Error> {
        require_not_paused(&e, PauseType::TRANSFER)?;
        from.require_auth();

        let from_key = DataKey::Balance(from.clone());
        let to_key = DataKey::Balance(to.clone());

        let from_balance: i128 = e.storage().persistent().get(&from_key).unwrap_or(0);
        if from_balance < amount {
            return Err(Error::InsufficientBalance);
        }

        e.storage()
            .persistent()
            .set(&from_key, &(from_balance - amount));
        e.storage().persistent().extend_ttl(&from_key, 100, 100);

        let to_balance: i128 = e.storage().persistent().get(&to_key).unwrap_or(0);
        e.storage()
            .persistent()
            .set(&to_key, &(to_balance + amount));
        e.storage().persistent().extend_ttl(&to_key, 100, 100);

        Ok(())
    }

    pub fn approve(
        e: Env,
        from: Address,
        spender: Address,
        amount: i128,
        expiration_ledger: u32,
    ) -> Result<(), Error> {
        from.require_auth();

        let key = DataKey::Allowance(AllowanceDataKey {
            from: from.clone(),
            spender: spender.clone(),
        });
        e.storage().persistent().set(
            &key,
            &AllowanceValue {
                amount,
                expiration_ledger,
            },
        );
        e.storage().persistent().extend_ttl(&key, 100, 100);

        Ok(())
    }

    pub fn allowance(e: Env, from: Address, spender: Address) -> i128 {
        let key = DataKey::Allowance(AllowanceDataKey { from, spender });
        match e.storage().persistent().get::<_, AllowanceValue>(&key) {
            Some(a) if e.ledger().sequence() <= a.expiration_ledger => a.amount,
            _ => 0,
        }
    }

    pub fn transfer_from(
        e: Env,
        spender: Address,
        from: Address,
        to: Address,
        amount: i128,
    ) -> Result<(), Error> {
        require_not_paused(&e, PauseType::TRANSFER)?;
        spender.require_auth();

        let current_allowance = Self::allowance(e.clone(), from.clone(), spender.clone());
        if current_allowance < amount {
            return Err(Error::InsufficientAllowance);
        }

        let new_allowance = current_allowance - amount;
        let key = DataKey::Allowance(AllowanceDataKey {
            from: from.clone(),
            spender: spender.clone(),
        });

        if new_allowance > 0 {
            let current_val = e
                .storage()
                .persistent()
                .get::<_, AllowanceValue>(&key)
                .unwrap();
            e.storage().persistent().set(
                &key,
                &AllowanceValue {
                    amount: new_allowance,
                    expiration_ledger: current_val.expiration_ledger,
                },
            );
            e.storage().persistent().extend_ttl(&key, 100, 100);
        } else {
            e.storage().persistent().remove(&key);
        }

        Self::transfer(e, from, to, amount)
    }
}
