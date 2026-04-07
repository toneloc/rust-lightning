# LDK-Node Dual-Funding Integration Sketch

## Overview

ldk-node already has all the building blocks. The integration requires changes in 3 files:
- `src/config.rs` — enable the feature flag
- `src/lib.rs` — add outbound v2 channel API
- `src/event.rs` — handle inbound v2 channels + signing (signing already works)

---

## 1. Config: Enable dual-funded channels

**File**: `src/config.rs`

```rust
// In default_user_config():
pub(crate) fn default_user_config(config: &Config) -> UserConfig {
    let mut user_config = UserConfig::default();
    user_config.channel_handshake_limits.force_announced_channel_preference = false;
    user_config.channel_handshake_config.negotiate_anchors_zero_fee_htlc_tx =
        config.anchor_channels_config.is_some();
    user_config.reject_inbound_splices = false;

    // NEW: Enable dual-funded channels if configured
    user_config.enable_dual_funded_channels = config.enable_dual_funded_channels;

    // ... rest unchanged
}
```

**File**: `src/config.rs` — add to Config struct:

```rust
pub struct Config {
    // ... existing fields ...

    /// If set to `true`, the node will advertise support for and accept dual-funded
    /// (V2) channel opens. This allows both parties to contribute funds to a channel.
    ///
    /// Default: `false`
    pub enable_dual_funded_channels: bool,
}
```

---

## 2. Outbound API: `open_dual_funded_channel()`

**File**: `src/lib.rs`

```rust
/// Opens a dual-funded (V2) channel to the given peer.
///
/// Unlike [`open_channel`], this uses the V2 channel establishment protocol where
/// both parties can contribute funds. The node's wallet automatically handles
/// coin selection and signing.
///
/// After calling this, the interactive transaction construction protocol runs
/// automatically. Once complete, the funding transaction is signed and broadcast.
/// The channel becomes usable after sufficient confirmations.
pub fn open_dual_funded_channel(
    &self,
    node_id: PublicKey,
    address: SocketAddress,
    channel_amount_sats: u64,
    announce_for_forwarding: bool,
) -> Result<UserChannelId, Error> {
    // Validate node is running
    let rt_lock = self.runtime.read().unwrap();
    let runtime = rt_lock.as_ref().ok_or(Error::NotRunning)?;

    // Connect to peer if needed
    let peer_info = PeerInfo { node_id, address };
    let con_peer_info = peer_info.clone();
    runtime.block_on(async {
        self.connection_manager.connect_peer_if_necessary(
            con_peer_info.node_id, con_peer_info.address
        ).await
    })?;

    // Wallet coin selection: select UTXOs for the funding amount
    let cur_anchor_reserve = total_anchor_channels_reserve_sats(
        &self.channel_manager, &self.config
    );
    let funding_inputs = self.wallet.select_funding_inputs(
        channel_amount_sats, cur_anchor_reserve
    )?;

    // Build config
    let mut user_config = default_user_config(&self.config);
    user_config.channel_handshake_config.announce_for_forwarding = announce_for_forwarding;

    // Generate user channel ID
    let user_channel_id: u128 = u128::from_ne_bytes(
        self.keys_manager.get_secure_random_bytes()[..16].try_into().unwrap()
    );

    // Call our new create_v2_channel API
    self.channel_manager.create_v2_channel(
        node_id,
        channel_amount_sats,
        funding_inputs,
        user_channel_id,
        Some(user_config),
        ConfirmationTarget::NonAnchorChannelFee,
    ).map_err(|e| {
        log_error!(self.logger, "Failed to open dual-funded channel: {:?}", e);
        Error::ChannelCreationFailed
    })?;

    // Store peer info
    self.peer_store.add_peer(peer_info)?;

    Ok(UserChannelId(user_channel_id))
}
```

**Wallet helper** — `src/wallet/mod.rs`:

```rust
/// Selects confirmed UTXOs from the wallet to fund a v2 channel.
/// Returns a Vec<FundingTxInput> suitable for create_v2_channel().
pub(crate) fn select_funding_inputs(
    &self, funding_amount_sats: u64, cur_anchor_reserve_sats: u64,
) -> Result<Vec<FundingTxInput>, Error> {
    let locked_wallet = self.inner.lock().unwrap();

    // Use the existing list_confirmed_utxos to get available UTXOs
    let utxos = self.list_confirmed_utxos_inner(&locked_wallet)?;

    // Simple greedy selection: pick UTXOs until we have enough
    // (funding amount + estimated fees + anchor reserve)
    let target = funding_amount_sats + cur_anchor_reserve_sats + 5_000; // fee buffer
    let mut selected = Vec::new();
    let mut total = 0u64;

    for utxo in utxos {
        if total >= target {
            break;
        }
        // Get the previous transaction for this UTXO
        let prevtx = locked_wallet.get_tx(utxo.outpoint.txid)
            .ok_or(Error::InsufficientFunds)?
            .tx_node
            .tx
            .clone();

        let funding_input = FundingTxInput::new_p2wpkh(
            (*prevtx).clone(), utxo.outpoint.vout
        ).map_err(|_| Error::InsufficientFunds)?;

        total += utxo.txout.value.to_sat();
        selected.push(funding_input);
    }

    if total < funding_amount_sats {
        return Err(Error::InsufficientFunds);
    }

    Ok(selected)
}
```

---

## 3. Inbound: Handle V2 channel requests with optional contribution

**File**: `src/event.rs` — modify the `OpenChannelRequest` handler:

```rust
LdkEvent::OpenChannelRequest {
    temporary_channel_id,
    counterparty_node_id,
    funding_satoshis,
    channel_type,
    is_announced,
    params,
    ..
} => {
    // ... existing validation (announced check, anchor check, reserve check) ...
    // ... existing LSP config overrides ...

    let allow_0conf = self.config.trusted_peers_0conf.contains(&counterparty_node_id);

    // NEW: Check if this is a dual-funded channel and if we should contribute
    let is_dual_funded = matches!(params, InboundChannelFunds::DualFunded { .. });

    let res = if is_dual_funded && self.config.dual_fund_contribution_sats > 0 {
        // Contribute to the dual-funded channel
        let contribution = self.config.dual_fund_contribution_sats;
        let funding_inputs = self.wallet.select_funding_inputs(
            contribution,
            total_anchor_channels_reserve_sats(&self.channel_manager, &self.config),
        ).unwrap_or_default();

        if funding_inputs.is_empty() {
            // Fall back to zero-contribution acceptance
            self.channel_manager.accept_inbound_channel(
                &temporary_channel_id,
                &counterparty_node_id,
                user_channel_id,
                channel_override_config,
            )
        } else {
            self.channel_manager.accept_inbound_channel_with_contribution(
                &temporary_channel_id,
                &counterparty_node_id,
                user_channel_id,
                contribution,
                funding_inputs,
                channel_override_config,
            )
        }
    } else if allow_0conf {
        self.channel_manager.accept_inbound_channel_from_trusted_peer_0conf(
            &temporary_channel_id,
            &counterparty_node_id,
            user_channel_id,
            channel_override_config,
        )
    } else {
        self.channel_manager.accept_inbound_channel(
            &temporary_channel_id,
            &counterparty_node_id,
            user_channel_id,
            channel_override_config,
        )
    };

    // ... existing error handling ...
}
```

---

## 4. Signing: Already works — no changes needed

The existing `FundingTransactionReadyForSigning` handler in `event.rs` already does:

```rust
// This handles BOTH v1 and v2 channel signing identically:
LdkEvent::FundingTransactionReadyForSigning {
    channel_id, counterparty_node_id, unsigned_transaction, ..
} => match self.wallet.sign_owned_inputs(unsigned_transaction) {
    Ok(partially_signed_tx) => {
        self.channel_manager.funding_transaction_signed(
            &channel_id, &counterparty_node_id, partially_signed_tx
        )
    }
    // ...
}
```

This works because:
- `sign_owned_inputs()` iterates all inputs in the unsigned tx
- For each input, it checks if the wallet owns it (via `locked_wallet.get_utxo()`)
- If owned, it signs it; if not owned (counterparty's input), it skips it
- The partially-signed tx is passed to `funding_transaction_signed()` which handles the rest

---

## 5. Confirmation + channel_ready: Already works — no changes needed

The existing handlers for `ChannelPending` and `ChannelReady` work for both v1 and v2 channels.
The chain monitoring, block connection, and `channel_ready` message exchange all use the same
code path regardless of how the channel was established.

---

## Summary of changes

| File | Change | Lines (est.) |
|------|--------|-------------|
| `src/config.rs` | Add `enable_dual_funded_channels` field + wire into `default_user_config()` | ~10 |
| `src/lib.rs` | Add `open_dual_funded_channel()` method | ~50 |
| `src/wallet/mod.rs` | Add `select_funding_inputs()` helper | ~40 |
| `src/event.rs` | Modify `OpenChannelRequest` to optionally contribute | ~30 |
| `src/config.rs` | Add `dual_fund_contribution_sats` config field | ~5 |
| **Total** | | **~135 lines** |

No changes needed for:
- `FundingTransactionReadyForSigning` handling (already works)
- Confirmation tracking / `channel_ready` (already works)
- Commitment signed exchange (already works)
- tx_signatures exchange (already works)
- Funding tx broadcast (already works)

The entire interactive-tx protocol, commitment exchange, and signature exchange
are handled automatically by rust-lightning's ChannelManager — ldk-node just needs
to provide the initial UTXOs and sign when asked.

---

## Prerequisites: rust-lightning changes required

ldk-node pins a specific rust-lightning commit. Before this integration can work,
the following rust-lightning changes must land upstream:

1. **`create_v2_channel()` API** — the new public method on ChannelManager
2. **`handle_accept_channel_v2()` implementation** — replaces the stub that always rejects
3. **`accept_inbound_channel_with_contribution()` API** — for inbound dual-fund with contribution
4. **`initial_commitment_signed_v2` bug fix** — the missing `monitor_pending_tx_signatures = true`
   line that prevents the acceptor from ever sending `tx_signatures`. Without this fix, dual-funded
   channels hang after commitment_signed exchange and never complete.

ldk-node currently pins to a March 2026 commit. Our changes are on top of upstream `main` at
`790ffa380` and would need to be rebased onto whatever commit ldk-node targets.

---

## Risk areas and what to test

### Coin selection producing valid interactive-tx inputs

The `select_funding_inputs()` sketch uses BDK's wallet to get UTXOs and wraps them as
`FundingTxInput::new_p2wpkh()`. The interactive-tx constructor validates inputs strictly:

- Each input must reference a valid previous transaction
- The `prevtx` must contain the output at the specified vout
- The output script must be P2WPKH (for `new_p2wpkh`)
- The weight estimate must be accurate

BDK's wallet stores full transactions, so `locked_wallet.get_tx(txid)` should return the
complete prevtx. But this is the most likely integration failure point — if BDK returns
a different transaction format or the vout doesn't match, the interactive-tx constructor
will reject the input.

**Test**: Create a regtest ldk-node, fund the on-chain wallet, call `open_dual_funded_channel()`,
and verify the interactive-tx negotiation completes without input validation errors.

### Signing inputs the wallet doesn't own

During interactive-tx, both parties' inputs are assembled into a single unsigned transaction.
`sign_owned_inputs()` iterates all inputs and only signs those the wallet recognizes
(via `locked_wallet.get_utxo(outpoint)`). Counterparty inputs are skipped. This is the
correct behavior — the counterparty signs their own inputs via their `tx_signatures` message.

This flow is already proven by splicing, which uses the identical code path.

### Interop with CLN

CLN supports dual-funding and liquidity ads. Two concerns:

1. **Feature negotiation**: Both sides must advertise `option_dual_fund` in their `init`
   message. rust-lightning does this when `enable_dual_funded_channels` is true. CLN
   enables it by default.

2. **Liquidity ads (`option_will_fund`)**: CLN can advertise rates for providing inbound
   liquidity. We don't implement liquidity ads — if CLN expects a lease fee, our node
   won't provide one. This won't break the protocol (it's optional), but CLN's funder
   plugin may decline to contribute if no lease is requested. Dual-funded channels
   where the CLN side contributes zero should work fine.

3. **`require_confirmed_inputs`**: CLN enforces this flag. Our implementation parses
   the field but doesn't validate inputs against chain state. If CLN sets
   `require_confirmed_inputs` and we send an unconfirmed UTXO, CLN will reject.
   Since ldk-node's wallet only uses confirmed UTXOs (`list_confirmed_utxos`), this
   should be fine in practice.

### Disconnect during negotiation

If either peer disconnects during interactive-tx negotiation, the unfunded v2 channel
is dropped (not persisted, not resumable). This is by design. The user would need to
call `open_dual_funded_channel()` again. ldk-node should handle the `ChannelClosed`
event gracefully — the wallet's UTXOs were never spent on-chain, so they remain available.

### Restart during negotiation

Same as disconnect — unfunded v2 channels are not persisted. On restart, the channel
is gone. No funds are at risk since nothing was broadcast. The user starts over.
