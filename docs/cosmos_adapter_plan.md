# Cosmos SDK Adapter Plan (ABCI++)

This document maps the current Rust Axum server endpoints and semantics to Cosmos SDK ABCI++ interfaces and proposes an adapter strategy.

## Goals
- Preserve current business logic (intents, MEV auction, scheduler) while enabling a path to a Cosmos SDK app-chain.
- Maintain end-to-end tests and observability.
- Incrementally integrate: start with offline adapter and progress to live ABCI++.

## Current Interfaces (Axum)
- ABCI-like endpoints (feature `abci`):
  - POST `/abci/prepare_proposal` { algorithm, fee_rate, max_intents? }
  - POST `/abci/process_proposal` { algorithm, fee_rate }
- Core endpoints: `/intents`, `/build_block`, `/metrics`, `/ws`, etc.
- MEV endpoints (feature `mev`): `/auction/submit*`, `/auction/book`, `/auction/clear`, `/auction/results`.
- Scheduler (feature `scheduler`): emits `scheduled_block_built` via WS.

## Mapping to ABCI++
- PrepareProposal(RequestPrepareProposal) -> determines block contents and app-specific ordering/payouts.
  - Map to Axum `/abci/prepare_proposal` payload:
    - `algorithm` -> greedy|fifo selection in server
    - `fee_rate` -> sequencer fee (surplus split)
    - Return intents/txs and app-side metadata
- ProcessProposal(RequestProcessProposal) -> validate proposed block and app rules.
  - Map to Axum `/abci/process_proposal` with same parameters; validate consistency and return ACCEPT/REJECT with codes.
- FinalizeBlock/Commit -> currently simulated; to be implemented when integrating with SDK state and KV stores.

## Adapter Strategy
1) Transport: ABCI++ over CometBFT (Unix socket or TCP). Rust side can use:
   - tendermint-rs/abci for Rust adapter, or implement minimal ABCI++ socket protocol.
2) Data model:
   - Define a canonical Tx format for intents and auction orders (protobuf via `prost`).
   - Map server-side JSON structs to protobuf for ABCI.
3) Execution flow:
   - ABCI PrepareProposal calls into existing Rust selection and payout logic (reuse functions behind feature `abci`).
   - ABCI ProcessProposal performs validations equivalent to current Axum endpoint.
4) State & persistence:
   - Start stateless (ephemeral, in-memory), then add KV (SDK store), keyed by intent IDs and orders.
5) Events:
   - Map WS events to ABCI events: `intent_submitted`, `block_built`, `auction_*` to ABCI `Event` types.

## Milestones
- M0: Protobuf definitions for intents, orders, and events; generate Rust types (prost-build) in `rust/server`.
- M1: ABCI socket shim (standalone bin) that forwards Prepare/Process to in-proc Rust handlers (same logic used by Axum).
- M2: Conformance tests: extend current `server/tests/abci_conformance.rs` to drive ABCI shim via socket.
- M3: Persistence: add simple height-indexed in-memory store; then explore SDK KV integration.
- M4: Observability: ABCI metrics and event emission parity.

## Risks & Notes
- ABCI++ API evolution; ensure compatibility with CometBFT version.
- Performance: additional serialization; mitigate by zero-copy and pre-allocated buffers.
- Determinism: ensure identical selection under same input; avoid non-deterministic RNG in PrepareProposal.

## Next Actions
- [ ] Add protobuf schema files for intents/orders/events.
- [ ] Add `build.rs` to generate prost code.
- [ ] Create `abci-shim` crate or bin target in `rust/server` behind feature `abci`.
- [ ] Extend integration tests to cover ABCI socket flow.
