#![no_std]
use emergency_guard::{EmergencyGuard, PauseType};
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, vec, Address, Env, String, Vec,
};

#[cfg(test)]
mod fuzz_test;
#[cfg(test)]
mod test;

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors returned by the `LiquidityPool` contract.
///
/// Code assignments are stable on-chain ABI — do NOT renumber.
///
/// | Code | Meaning                |
/// |------|------------------------|
/// |  1   | AlreadyInitialized     |
/// |  2   | NotInitialized         |
/// |  3   | Unauthorized           |
/// |  4   | InsufficientBalance    |
/// |  5   | InsufficientLiquidity  |
/// |  6   | InsufficientShares     |
/// |  7   | InsufficientAllowance  |
/// |  8   | SlippageExceeded       |
/// |  9   | InvalidFee             |
/// | 10   | OracleNotConfigured    |
/// | 11   | PendingFeeUpdateExists |
/// | 12   | TimelockNotElapsed     |
/// | 13   | NoPendingFeeUpdate     |
/// | 14   | Paused                 |
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

// ── Granular pause operation bitmasks ─────────────────────────────────────────

/// Bitmask constants for individual pausable operations.
///
/// Each bit controls one operation independently, enabling surgical pausing
/// (e.g., halt only swaps while deposits remain open).
pub mod pause_op {
    pub const SWAP: u32 = 1 << 0;
    pub const DEPOSIT: u32 = 1 << 1;
    pub const WITHDRAW: u32 = 1 << 2;
    pub const TRANSFER: u32 = 1 << 3;
    pub const MINT: u32 = 1 << 4;
    pub const BURN: u32 = 1 << 5;
    pub const ALL: u32 = u32::MAX;
}

// ── Oracle interface ──────────────────────────────────────────────────────────

#[soroban_sdk::contractclient(name = "PriceOracleClient")]
pub trait PriceOracle {
    fn latest_price(e: Env) -> i128;
}

// ── On-chain data types ───────────────────────────────────────────────────────

/// Aggregated pool state – stored as a single instance-storage value to
/// minimize ledger I/O on mutable operations.
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

/// Queued fee change waiting for the timelock to elapse.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingFeeUpdate {
    pub new_fee_bps: i128,
    pub executable_after_ledger: u32,
}

/// Oracle configuration persisted on-chain.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OracleConfig {
    pub oracle_id: Address,
    pub base_fee_bps: i128,
    pub timelock_ledgers: u32,
    pub based_on_volatility_bps: i128,
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
    /// Aggregated pool state (token addresses, reserves, total shares, fee).
    Pool,
    /// Primary admin address (for fee management; also seeded into guard admins).
    Admin,
    /// Multi-sig admin list for the inline EmergencyGuard.
    GuardAdmins,
    /// Number of signatures required for guarded operations.
    GuardThreshold,
    /// Bitmask of currently paused operations (`PauseOp` constants).
    GuardPauseState,
    /// Per-user LP-share balance (persistent storage).
    Balance(Address),
    /// ERC-20-style spend allowance (persistent storage).
    Allowance(AllowanceDataKey),
    /// Oracle feed configuration.
    OracleConfig,
    /// Last sampled oracle price for volatility calculation.
    LastOraclePrice,
    /// Last computed volatility in bps.
    LastVolatilityBps,
    /// Scheduled fee change awaiting timelock expiry.
    PendingFeeUpdate,
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Integer square-root via Newton's method — no `std`, no floats.
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

/// Persist PoolState back to instance storage.
fn save_pool(e: &Env, pool: &PoolState) {
    e.storage().instance().set(&DataKey::Pool, pool);
}

fn check_paused(pool: &PoolState) -> Result<(), Error> {
    if pool.paused {
        Err(Error::Paused)
    } else {
        Ok(())
    }
}

fn check_not_operation_paused(e: &Env, operation: u32) -> Result<(), Error> {
    if EmergencyGuard::is_paused(e.clone(), operation) {
        Err(Error::Paused)
    } else {
        Ok(())
    }
}

/// Map volatility (bps) to a dynamic fee target.
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

// ── Inline EmergencyGuard helpers ─────────────────────────────────────────────
//
// The guard logic is implemented directly here (rather than through a crate
// dependency) to avoid wasm export symbol conflicts: the `emergency_guard`
// contract crate also marks its own public functions as wasm exports via
// `#[contractimpl]`, causing linker errors when both are in the same binary.

/// Initialise the inline guard with a single admin and threshold of 1.
fn guard_init(e: &Env, admin: Address) {
    let admins = vec![e, admin];
    e.storage().instance().set(&DataKey::GuardAdmins, &admins);
    e.storage().instance().set(&DataKey::GuardThreshold, &1u32);
    e.storage().instance().set(&DataKey::GuardPauseState, &0u32);
}

/// Read the current pause-state bitmask.
fn guard_pause_state(e: &Env) -> u32 {
    e.storage()
        .instance()
        .get(&DataKey::GuardPauseState)
        .unwrap_or(0u32)
}

/// Return `Err(Error::Paused)` if `op` is currently paused.
fn guard_check_not_paused(e: &Env, op: u32) -> Result<(), Error> {
    if guard_pause_state(e) & op != 0 {
        Err(Error::Paused)
    } else {
        Ok(())
    }
}

/// Return `true` if `addr` is in the admin list.
fn guard_is_admin(e: &Env, addr: &Address) -> bool {
    let admins: Vec<Address> = e
        .storage()
        .instance()
        .get(&DataKey::GuardAdmins)
        .unwrap_or_else(|| Vec::new(e));
    admins.iter().any(|a| a == *addr)
}

/// Require that `caller` is an admin and has signed (`require_auth`).
fn guard_require_admin(e: &Env, caller: &Address) -> Result<(), Error> {
    caller.require_auth();
    if !guard_is_admin(e, caller) {
        return Err(Error::Unauthorized);
    }
    Ok(())
}

/// Require that enough unique admins have signed (`require_auth` on each).
fn guard_require_multisig(e: &Env, approvers: &Vec<Address>) -> Result<(), Error> {
    let threshold: u32 = e
        .storage()
        .instance()
        .get(&DataKey::GuardThreshold)
        .ok_or(Error::NotInitialized)?;

    let mut valid: u32 = 0;
    let mut seen: Vec<Address> = Vec::new(e);

    for addr in approvers.iter() {
        if seen.iter().any(|a| a == addr) {
            continue;
        }
        seen.push_back(addr.clone());
        if guard_is_admin(e, &addr) {
            addr.require_auth();
            valid += 1;
        }
    }

    if valid < threshold {
        return Err(Error::Unauthorized);
    }
    Ok(())
}

/// Set or clear specific operation bits in the pause bitmask.
fn guard_set_ops(e: &Env, ops: u32, paused: bool) {
    let mut state = guard_pause_state(e);
    if paused {
        state |= ops;
    } else {
        state &= !ops;
    }
    e.storage()
        .instance()
        .set(&DataKey::GuardPauseState, &state);
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct LiquidityPool;

#[contractimpl]
impl LiquidityPool {
    // ── Initialisation ────────────────────────────────────────────────────────

    /// One-time pool setup.  Bootstraps the inline EmergencyGuard with the
    /// supplied admin as the sole initial signer (threshold = 1).
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

        guard_init(&e, admin);
        Ok(())
    }

    // ── Granular pause (EmergencyGuard admin interface) ───────────────────────

    /// Pause or unpause **all** core operations at once.
    ///
    /// Any single admin can call this; it requires only the admin's signature.
    pub fn set_paused(e: Env, paused: bool) -> Result<(), Error> {
        let admin: Address = e
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)?;
        guard_require_admin(&e, &admin)?;
        let all = pause_op::SWAP | pause_op::DEPOSIT | pause_op::WITHDRAW | pause_op::BURN;
        guard_set_ops(&e, all, paused);
        Ok(())
    }

    /// Pause only swap operations.
    pub fn pause_swaps(e: Env) -> Result<(), Error> {
        let admin: Address = e
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)?;
        guard_require_admin(&e, &admin)?;
        guard_set_ops(&e, pause_op::SWAP, true);
        Ok(())
    }

    /// Resume swap operations.
    pub fn resume_swaps(e: Env) -> Result<(), Error> {
        let admin: Address = e
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)?;
        guard_require_admin(&e, &admin)?;
        guard_set_ops(&e, pause_op::SWAP, false);
        Ok(())
    }

    /// Pause only deposit operations.
    pub fn pause_deposits(e: Env) -> Result<(), Error> {
        let admin: Address = e
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)?;
        guard_require_admin(&e, &admin)?;
        guard_set_ops(&e, pause_op::DEPOSIT, true);
        Ok(())
    }

    /// Resume deposit operations.
    pub fn resume_deposits(e: Env) -> Result<(), Error> {
        let admin: Address = e
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)?;
        guard_require_admin(&e, &admin)?;
        guard_set_ops(&e, pause_op::DEPOSIT, false);
        Ok(())
    }

    /// Pause only withdrawal operations.
    pub fn pause_withdrawals(e: Env) -> Result<(), Error> {
        let admin: Address = e
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)?;
        guard_require_admin(&e, &admin)?;
        guard_set_ops(&e, pause_op::WITHDRAW, true);
        Ok(())
    }

    /// Resume withdrawal operations.
    pub fn resume_withdrawals(e: Env) -> Result<(), Error> {
        let admin: Address = e
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)?;
        guard_require_admin(&e, &admin)?;
        guard_set_ops(&e, pause_op::WITHDRAW, false);
        Ok(())
    }

    /// Emergency: pause **all** operations (requires multi-sig approval).
    pub fn emergency_pause_all(e: Env, approvers: Vec<Address>) -> Result<(), Error> {
        guard_require_multisig(&e, &approvers)?;
        guard_set_ops(&e, pause_op::ALL, true);
        Ok(())
    }

    /// Resume all paused operations (requires multi-sig approval).
    pub fn resume_all(e: Env, approvers: Vec<Address>) -> Result<(), Error> {
        guard_require_multisig(&e, &approvers)?;
        guard_set_ops(&e, pause_op::ALL, false);
        Ok(())
    }

    /// Returns the raw pause-state bitmask.
    pub fn get_pause_state(e: Env) -> u32 {
        guard_pause_state(&e)
    }

    /// Returns `true` when `operation` is currently paused.
    pub fn is_paused_op(e: Env, operation: u32) -> bool {
        guard_pause_state(&e) & operation != 0
    }

    /// Returns the list of authorized guard admins.
    pub fn get_guard_admins(e: Env) -> Vec<Address> {
        e.storage()
            .instance()
            .get(&DataKey::GuardAdmins)
            .unwrap_or_else(|| Vec::new(&e))
    }

    /// Returns the current multi-sig approval threshold.
    pub fn get_guard_threshold(e: Env) -> u32 {
        e.storage()
            .instance()
            .get(&DataKey::GuardThreshold)
            .unwrap_or(0)
    }

    /// Add a new admin (requires multi-sig approval).
    pub fn add_guard_admin(
        e: Env,
        approvers: Vec<Address>,
        new_admin: Address,
    ) -> Result<(), Error> {
        guard_require_multisig(&e, &approvers)?;
        let mut admins: Vec<Address> = e
            .storage()
            .instance()
            .get(&DataKey::GuardAdmins)
            .unwrap_or_else(|| Vec::new(&e));
        if !admins.iter().any(|a| a == new_admin) {
            admins.push_back(new_admin);
            e.storage().instance().set(&DataKey::GuardAdmins, &admins);
        }
                base_fee_bps: DEFAULT_BASE_FEE_BPS,
                admin,
                paused: false,
            },
        );
        EmergencyGuard::initialize(e.clone(), vec![&e, admin], 1)
            .map_err(|_| Error::Unauthorized)?;
        Ok(())
    }

    /// Remove an admin (requires multi-sig approval; cannot drop below threshold).
    pub fn remove_guard_admin(
        e: Env,
        approvers: Vec<Address>,
        admin: Address,
    ) -> Result<(), Error> {
        guard_require_multisig(&e, &approvers)?;
        let admins: Vec<Address> = e
            .storage()
            .instance()
            .get(&DataKey::GuardAdmins)
            .unwrap_or_else(|| Vec::new(&e));
        let threshold: u32 = e
            .storage()
            .instance()
            .get(&DataKey::GuardThreshold)
            .unwrap_or(1);
        if admins.len() as u32 <= threshold {
            return Err(Error::Unauthorized);
        }
        let mut new_admins: Vec<Address> = Vec::new(&e);
        for a in admins.iter() {
            if a != admin {
                new_admins.push_back(a);
            }
        }
        e.storage()
            .instance()
            .set(&DataKey::GuardAdmins, &new_admins);
        Ok(())
            .get::<_, PoolState>(&DataKey::Pool)
            .map(|p| p.fee_bps)
            .unwrap_or(DEFAULT_BASE_FEE_BPS)
    }

    // ── Fee management ────────────────────────────────────────────────────────

    /// Returns the current swap fee in basis points.
    pub fn get_fee(e: Env) -> i128 {
        load_pool(&e)
            .map(|p| p.fee_bps)
            .unwrap_or(DEFAULT_BASE_FEE_BPS)
    }

    /// Admin-only: set the swap fee directly.  Valid range: 0–100 bps.
    pub fn set_fee(e: Env, fee_bps: i128) -> Result<(), Error> {
        if !(0..=MAX_FEE_BPS).contains(&fee_bps) {
            return Err(Error::InvalidFee);
        }
        // One read + one write instead of 3 reads + 2 writes.
        let mut pool = load_pool(&e)?;
        pool.admin.require_auth();
        let old_fee = pool.fee_bps;
        pool.fee_bps = fee_bps;
        pool.base_fee_bps = fee_bps;
        save_pool(&e, &pool);
        e.events().publish(
            (String::from_str(&e, "fee_changed"), pool.admin.clone()),
            FeeChangedEvent {
                admin: pool.admin,
                old_fee_bps: old_fee,
                new_fee_bps: fee_bps,
            },
        );
        Ok(())
    }

    /// Admin-only: configure external oracle and timelock parameters.
    pub fn configure_fee_oracle(
        e: Env,
        oracle: Address,
        base_fee_bps: i128,
        timelock_ledgers: u32,
    ) -> Result<(), Error> {
        if !(0..=MAX_FEE_BPS).contains(&base_fee_bps) {
            return Err(Error::InvalidFee);
        }
        // One read (pool) + one write (oracle config) instead of 1 read + 3 writes.
        let mut pool = load_pool(&e)?;
        pool.admin.require_auth();
        pool.base_fee_bps = base_fee_bps;
        save_pool(&e, &pool);

        let cfg = OracleConfig {
            oracle,
            last_price: 0,
            last_volatility_bps: 0,
            timelock_ledgers,
        };
        e.storage().instance().set(&DataKey::Oracle, &cfg);
        Ok(())
    }

    pub fn get_last_volatility_bps(e: Env) -> i128 {
        e.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)?;
        admin.require_auth();

        let mut pool = load_pool(&e)?;
        let old_fee = pool.fee_bps;
        pool.fee_bps = fee_bps;
        save_pool(&e, &pool);
            .get::<_, OracleConfig>(&DataKey::Oracle)
            .map(|c| c.last_volatility_bps)
            .unwrap_or(0)
    }

    pub fn get_pending_fee_update(e: Env) -> Option<PendingFeeUpdate> {
        e.storage().instance().get(&DataKey::PendingFeeUpdate)
    }

    /// Pulls price from oracle, computes volatility and schedules a timelocked fee update.
    pub fn sync_fee_from_oracle(e: Env) -> Result<Option<PendingFeeUpdate>, Error> {
        // One read (oracle config) instead of 5 separate reads.
        let mut cfg: OracleConfig = e
            .storage()
            .instance()
            .get(&DataKey::Oracle)
            .ok_or(Error::OracleNotConfigured)?;

        let oracle_client = PriceOracleClient::new(&e, &cfg.oracle);
        let current_price = oracle_client.latest_price();
        if current_price <= 0 {
            return Err(Error::InvalidOraclePrice);
        }

        let prev = cfg.last_price;
        cfg.last_price = current_price;

        if prev <= 0 {
            cfg.last_volatility_bps = 0;
            // One write instead of 2 writes.
            e.storage().instance().set(&DataKey::Oracle, &cfg);
            return Ok(None);
        }

        let price_delta = if current_price >= prev {
            current_price - prev
        } else {
            prev - current_price
        };
        let volatility_bps = price_delta
            .checked_mul(10_000)
            .ok_or(Error::InvalidOraclePrice)?
            / prev;

        cfg.last_volatility_bps = volatility_bps;
        // One write instead of 2 writes.
        e.storage().instance().set(&DataKey::Oracle, &cfg);

        let pool = load_pool(&e)?;
        let target_fee = target_fee_from_volatility(pool.base_fee_bps, volatility_bps);
        if target_fee == pool.fee_bps {
            return Ok(None);
        }

        let execute_after = e.ledger().sequence().saturating_add(cfg.timelock_ledgers);
        let pending = PendingFeeUpdate {
            new_fee_bps: target_fee,
            executable_after_ledger: execute_after,
            based_on_volatility_bps: volatility_bps,
        };
        e.storage()
            .instance()
            .set(&DataKey::PendingFeeUpdate, &pending);

        let scheduled_by = e.current_contract_address();
        e.events().publish(
            (
                String::from_str(&e, "fee_update_scheduled"),
                scheduled_by.clone(),
            ),
            FeeUpdateScheduledEvent {
                scheduled_by,
                old_fee_bps: pool.fee_bps,
                new_fee_bps: target_fee,
                executable_after_ledger: execute_after,
                volatility_bps,
            },
        );

        Ok(Some(pending))
    }

    /// Applies a previously scheduled fee update after timelock elapses.
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

        // One read + one write instead of 2 reads + 1 write.
        let mut pool = load_pool(&e)?;
        let old_fee = pool.fee_bps;
        pool.fee_bps = pending.new_fee_bps;
        save_pool(&e, &pool);
        e.storage().instance().remove(&DataKey::PendingFeeUpdate);

        e.events().publish(
            (String::from_str(&e, "fee_changed"), admin.clone()),
            FeeChangedEvent {
                admin,
                old_fee_bps: old_fee,
                new_fee_bps: pending.new_fee_bps,
            },
        );

        Ok(pending.new_fee_bps)
    }

    /// Admin-only: pause or unpause the pool.
    pub fn set_paused(e: Env, paused: bool) -> Result<(), Error> {
        let mut pool = load_pool(&e)?;
        pool.admin.require_auth();
        pool.paused = paused;
        save_pool(&e, &pool);
        Ok(())
    }

    // ── Oracle-driven dynamic fee ─────────────────────────────────────────────

    /// Admin-only: configure the price oracle for dynamic fee adjustment.
    pub fn configure_fee_oracle(
        e: Env,
        oracle_id: Address,
        base_fee_bps: i128,
        timelock_ledgers: u32,
    ) -> Result<(), Error> {
        let admin: Address = e
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)?;
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

    /// Sample the oracle price, compute volatility, and schedule a fee update
    /// when the target differs from the current fee.
    ///
    /// Returns `None` on the first call (seeds the baseline) or when
    /// volatility is below all thresholds.
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
            // Seed the baseline price — no update scheduled yet.
            e.storage()
                .instance()
                .set(&DataKey::LastOraclePrice, &current_price);
            return Ok(None);
        };

        // |Δ| / last expressed in basis-points.
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

    /// Apply the pending fee update once the timelock has elapsed.
    pub fn execute_fee_update(e: Env) -> Result<i128, Error> {
        let pending: PendingFeeUpdate = e
            .storage()
            .instance()
            .get(&DataKey::PendingFeeUpdate)
            .ok_or(Error::NoPendingFeeUpdate)?;

        if e.ledger().sequence() < pending.executable_after_ledger {
            return Err(Error::TimelockNotElapsed);
        }

        let mut pool = load_pool(&e)?;
        pool.fee_bps = pending.new_fee_bps;
        save_pool(&e, &pool);
        e.storage().instance().remove(&DataKey::PendingFeeUpdate);
        Ok(pending.new_fee_bps)
    }

    /// Returns the pending fee update, if any.
    pub fn get_pending_fee_update(e: Env) -> Option<PendingFeeUpdate> {
        e.storage().instance().get(&DataKey::PendingFeeUpdate)
    }

    /// Returns the last recorded price-move volatility in basis-points.
    pub fn get_last_volatility_bps(e: Env) -> i128 {
        e.storage()
            .instance()
            .get(&DataKey::LastVolatilityBps)
            .unwrap_or(0)
    }

    // ── Core AMM operations ───────────────────────────────────────────────────

    /// Deposit `amount_a` of token A and `amount_b` of token B; mint LP shares.
    pub fn deposit(e: Env, to: Address, amount_a: i128, amount_b: i128) -> Result<i128, Error> {
        guard_check_not_paused(&e, pause_op::DEPOSIT)?;
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

    /// Constant-product swap. `buy_a = true` → buy token A, sell token B.
    pub fn swap(e: Env, to: Address, buy_a: bool, out: i128, in_max: i128) -> Result<i128, Error> {
        guard_check_not_paused(&e, pause_op::SWAP)?;
        // One read instead of 5 separate reads.
        let mut pool = load_pool(&e)?;
        check_paused(&pool)?;
        check_not_operation_paused(&e, PauseType::SWAP)?;
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

    /// Burn LP shares and receive proportional reserves.
    pub fn withdraw(e: Env, to: Address, share_amount: i128) -> Result<(i128, i128), Error> {
        guard_check_not_paused(&e, pause_op::WITHDRAW)?;
        // One read instead of 4 separate reads.
        let mut pool = load_pool(&e)?;
        check_paused(&pool)?;
        check_not_operation_paused(&e, PauseType::WITHDRAW)?;
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

    /// Burn LP shares without withdrawing reserves (fee-burn / charity).
    pub fn burn(e: Env, from: Address, amount: i128) -> Result<(), Error> {
        guard_check_not_paused(&e, pause_op::BURN)?;
        let mut pool = load_pool(&e)?;
        check_paused(&pool)?;
        check_not_operation_paused(&e, PauseType::BURN)?;
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
