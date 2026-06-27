# Bridge crate

This crate implements a minimal, auditable validator-set rotation primitive for a cross-chain bridge.

Key features
- `Bridge` struct with an `epoch` counter and validator set
- `rotate_validators(new_set, epoch, proofs)` requires a quorum proof from the *current* validator set and advances the epoch atomically
- `validate_inbound_epoch(signed_epoch)` rejects messages signed by retired epochs (signed_epoch < current epoch)
- `set_inbound_cap(max_per_window, window_size, current_time)` / `admit_inbound(amount, current_time)` enforce a configurable, rolling-ledger-time cap on cumulative inbound value — defense-in-depth against an authorized-but-compromised validator set draining the bridge in a single window. **Fail-closed by default**: a fresh `Bridge` admits no inbound value until a cap is explicitly configured. See `SECURITY_NOTES.md` for the full threat model and design rationale.

Design notes
- Validator public keys are stored as raw bytes to keep on-disk/state serialization simple and unambiguous.
- Quorum threshold is a supermajority > 2/3 of current validators.
- Signatures are over a canonical payload: `bincode((new_set_bytes_vec, epoch))` — this binds the epoch to the new set.
- This crate is intentionally standalone (off-chain validator/quorum verification logic) and is not a member of the parent Soroban workspace; run `cargo test` from this directory.

See `src/lib.rs` for implementation and unit tests.
