# ZK Signal Intelligence (Placeholder)

This directory will contain the ZK circuits and tooling for the Shared Signal Intelligence layer (Phase 3).

Initial scope:

- Circom circuit(s) that allow a bot (Scout) to prove knowledge of a profitable opportunity without revealing private strategy details.
- Public inputs: coarse identifiers (e.g., DEX IDs) and an estimated profit tier bucket.
- Private inputs: exact asset path and amounts.
- Output: a verifiable proof that the opportunity exists within the declared tier.

Next steps (future PRs):

- Implement `signal.circom` circuit and test vectors.
- Add `snarkjs` scripts for setup, proving, and verification key generation.
- Provide a simple Solidity/Move ink! verifier example (chain TBD).

Note: The current repository does not include Circom or snarkjs in requirements to keep the prototype lightweight.
