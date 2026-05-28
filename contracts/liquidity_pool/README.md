# Liquidity Pool Contract

Constant-product AMM with LP shares, dynamic oracle fees, and **EmergencyGuard** granular pause controls.

## Emergency pause (bitmask)

Pause state is a single **`u32` bitmask** (4 bytes) shared with the `emergency_guard` crate, stored under `PauseState` in instance storage. Each operation is one bit (`PauseType::SWAP`, `DEPOSIT`, `WITHDRAW`, `BURN`, `TRANSFER`, etc.).

| Function | Description |
|----------|-------------|
| `guard_pause(admin, operation, paused)` | Pause/unpause one operation |
| `guard_is_paused(operation)` | Query one bit |
| `get_pause_state()` | Raw bitmask |
| `emergency_pause(approvers)` | Multi-sig pause all |
| `resume(approvers)` | Multi-sig clear all |
| `rotate_admin(approvers, old, new)` | Replace pool + guard admin |

Core AMM paths (`deposit`, `swap`, `withdraw`, `burn`, `transfer`) call `require_not_paused` before executing.

## Initialization

```rust
pool.initialize(admin, token_a, token_b)?;
// Bootstraps EmergencyGuard with [admin], threshold 1
```

## Fee admin

`DataKey::Admin` is the pool fee admin (may differ from guard admins after rotation). Use `set_fee`, `configure_fee_oracle`, `sync_fee_from_oracle`, and `execute_fee_update`.
