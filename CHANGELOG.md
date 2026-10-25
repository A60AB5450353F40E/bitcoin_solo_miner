# Changelog

Releases up to 0.1.2 are from the upstream Bitcoin miner this repository was
forked from; 0.2.0 is the first Bitcoin Cash release of this fork.

## [0.2.1] - 2026-10-25

### Added
- A payout line may be a raw scriptPubKey in hex prefixed `0x`, used verbatim as
  the coinbase output script. No address decoding, network check or type check
  happens on this form; it exists for output types this build cannot parse (bare
  multisig, nonstandard scripts, cashaddr until someone writes the decoder). The
  miner cannot tell whether such a script is spendable, so a malformed entry
  either burns the reward or donates it to whoever spends it first. An empty
  `0x` is accepted and produces a zero-length script, and the payload is capped
  at `MAX_SCRIPT_SIZE`, 10,000 bytes.
- The shipped `addresses.txt` now also contains `0x` and `0x51`.

### Changed
- The shipped `.env` sets `ADDRESS_MODE="loop"`, so a fresh clone keeps mining.
  The code default when the variable is unset is still `once`.

### Notes
- Every address in the shipped `addresses.txt` is anyone-can-spend on purpose,
  as a testnet fixture: `0x` locking script is spendable with `0x51` unlocking
  script and `0x51` with `0x`, and the P2SH address's redeem script is `0x51`,
  i.e. `OP_1`. Replace them before mining anywhere the coins have value.

## [0.2.0] - 2026-10-20

### Breaking
- __The crate and binary are now `bch_solo_miner`.__ Every `nohup` line, systemd
  unit and script that invokes `bitcoin_solo_miner` must be updated; the old
  name is not kept as an alias. Configuration, `.env` keys and on-disk file
  names are unchanged.
- __This release mines Bitcoin Cash, not Bitcoin.__ It refuses to start against a
  non-BCH node, and blocks are built with BCH rules: no witness commitment
  output, no SegWit transaction pass, legacy base58 payouts only.
- __`bc1…` payout addresses are rejected.__ bech32 does not exist on BCH and the
  resulting output would be spendable by anyone, so a file of mainnet Bitcoin
  addresses now fails at startup with "no usable payout addresses". Use the
  legacy base58 encoding (`1…` P2PKH or `3…` P2SH) of the same key.
- __The template must include `target_time`.__ The miner refuses a template
  without it rather than guessing a cadence, and logs the missing-field error
  instead of hashing. Needed because on a chain with the minimum-difficulty rule
  `nBits` depends on the block's own timestamp.
- __`ADDRESS_MODE` defaults to `once`.__ The miner walks the list and stops, so an
  unmodified `.env` mines one block per address and then idles. Continuous mining
  needs `ADDRESS_MODE="loop"`.
- __`PAYOUT_ADDRESSES` is read-only.__ The rotation cursor lives in
  `ADDRESSES_STATE` (default `<file>.state`), and a winning address is no longer
  removed from the file.
- The `[stats]` line changed shape: `height=Some(n) block~… stale~…` became
  `tau= find~ waste~ min_diff= time=`, plus `payout_set=`.

### Fixed
- __Mining never started against a realistic mempool.__ Template transactions were
  decoded with `bitcoin::consensus`, which does not understand the BCH CashTokens
  output prefix. Any template holding a token transaction failed to parse, was
  discarded, and nulled both caches, so the miner never hashed. Txids now come
  from the template's own field and transactions are kept as the template's
  raw bytes: the Merkle root only needs ids and the block body only needs bytes.
- The header timestamp is held at the minimum-difficulty threshold. On a chain
  with the minimum-difficulty rule `nBits` is a function of the block's own
  timestamp, so a header stamped past `parent_time + 2 * target_time`
  demanded `powLimit` bits while carrying the DAA's, which the node rejects as
  `bad-diffbits`. The threshold is derived from `target_time`, and the
  parent's own header time is read with `getblockheader` rather than inferred
  from `mintime`, which is a median time past.
- Every template transaction is included and `coinbasevalue` is claimed verbatim.
  Trimming the tx set while still claiming the full value produced
  `bad-cb-amount` on every candidate block.
- Transactions are kept in the template's own order. BCH enforces canonical
  transaction ordering (CTOR): every transaction after the coinbase must be
  sorted by txid, so any reordering makes the block invalid. `depends` is
  informational on BCH and is not used to sort — CTOR deliberately allows a
  transaction to spend an output created later in the same block, so a child may
  legitimately precede its parent.
- The transaction cache is rebuilt before a template is published, so workers can
  never mine a header whose `coinbasevalue` belongs to a different tx set.
- Block size is predicted from the real transaction count, coinbase included, and
  the assembled length must equal the prediction before submission; a mismatch or
  an oversized block is refused rather than warned about. The old arithmetic
  under-counted by one and would have picked a one-byte count varint for a
  253-transaction block.
- Two idle paths in the mining loop returned without sleeping, spinning at 100%
  CPU against a template with unusable `bits` or one that would not fit
  `sizelimit`.
- A payout address is retired only after `submitblock` accepts the block. The
  previous code removed it as soon as a hash met the target, so a stale or
  rejected candidate burned an address that never earned anything, and a
  `"duplicate"` result left the cursor untouched as well.
- An RPC error body is previewed by characters rather than bytes, so a body cut
  mid-codepoint cannot panic inside the error path.
- The address-file watcher survives a poisoned lock instead of silently
  disabling reloads for the rest of the process's life.
- The persisted address cursor is `fsync`ed before its rename, so a crash
  mid-write cannot leave a truncated or empty state file.
- Startup no longer leaves `PAYOUT_ADDRESS` unset for ten seconds, during which
  every worker returned without hashing.

### Changed
- Hashing was rewritten around a SHA256 midstate over the first 64 header bytes
  and `sha2`'s raw `compress256` (`compress` feature): block 1 is compressed
  once per batch and both padding blocks are built once, so an attempt costs two
  compressions with no hasher construction, allocation or state clone in the hot
  path. Together with the target-compare rewrite, this makes 0.2.0 roughly twice
  the hash rate of 0.1.x on the same hardware. The gain depends on SHA-NI (x86) or
  the SHA2 extension (aarch64), which `sha2` 0.10 selects at runtime.
- The target compare walks the digest and target in place instead of reversing a
  copy, so no 32-byte move per attempt.
- `target_time` is required in `getblocktemplate`, and the miner refuses a
  template without it rather than guessing a cadence. It is the cadence of the
  block being built, so it flips at the boundary block itself rather than one
  block later — which matters, because the minimum-difficulty threshold is
  evaluated for that same block.
- New `MIN_DIFFICULTY_RULE`: `auto` (default, on for every chain except mainnet),
  `on`, `off`. Nothing in the RPC surface exposes `fPowAllowMinDifficultyBlocks`,
  so `auto` infers it from the chain name; the override exists for a network
  whose targeting changes.
- Longpoll holds are measured against the minimum-difficulty boundary using the
  held template's own `curtime`, not the moment the request was issued: a
  template fetched 90 s before the boundary used to be held 2 * tau seconds past
  it, so the miner kept hashing a harder target than the network required for
  that whole window. The absolute ceiling is 1300 s, consecutive issues are
  spaced by at least 250 ms so an intermediary that answers longpolls instantly
  cannot become a request loop, and any `submitblock` result — accepted,
  duplicate or rejected — drops the hold immediately. A failed longpoll is
  retried after 30 fallback polls at 1 Hz rather than disabling itself for good.
- `NUM_TASKS` is clamped to `available_parallelism()`, which honours CPU affinity
  and cgroup quotas. Oversubscribing a CPU-bound miner only adds scheduler churn;
  the clamp is logged when it fires.
- Configuration is loaded from `.env` in the working directory, falling back to
  the directory holding the binary, with `DOTENV_PATH` overriding both. Multiple
  candidates are reported rather than silently resolved.
- The address file is read-only. The rotation cursor is persisted in
  `ADDRESSES_STATE` (default `<PAYOUT_ADDRESSES>.state`) together with the file's
  length and sha256, so an append keeps the cursor while a rewrite resets it.
- New `ADDRESS_MODE`: `once` (default, stops when exhausted), `loop`, `random`.
  Advancement happens on an accepted block only, not on a timer.
- Payout addresses are validated at startup: bech32 (`bc1…`) is rejected because
  it is anyone-can-spend on BCH, and entries that are not P2PKH/P2SH are skipped.
- Payout addresses are no longer required to be mainnet-network addresses. The
  script bytes are network-agnostic, so testnet and regtest legacy addresses work
  unchanged; nothing verifies that the address matches the node's chain.
- `coinbaseaux.flags` from the template are embedded in the coinbase scriptSig
  between the height push and the extra nonce, inside the same 100-byte budget.
- `getblocktemplate` requests `longpoll`, using the current tip hash as
  `longpollid`, with a fall back to one-second polling.
- Block size is checked against the template's `sizelimit` before hashing; the
  32 MB fallback is used only when `sizelimit` is absent.
- Startup refuses to proceed against a non-BCH daemon, or while the node reports
  initial block download, and logs the resolved `MIN_DIFFICULTY_RULE`.
- Stats report `tau`, `find~`, `waste~`, `min_diff` and the timestamp mode, so the
  cadence, the expected find time at the current difficulty and the proximity of
  the minimum-difficulty flip point are visible.
- `reqwest` uses rustls only (no OpenSSL); the unused `blocking` feature is gone.
- Packaging: `authors` no longer carries a bech32 address, `keywords` trimmed to
  the five crates.io allows, and `rust-version = "1.85"` declared.

### Removed
- The SegWit witness-commitment pass: dummy coinbase, wtxid Merkle tree and the
  38-byte `OP_RETURN` commitment output. It is meaningless on BCH and added 38
  bytes to every candidate block.
- The 0.1.1 advice to use `bc1q` payout addresses. That encoding does not exist on
  BCH and the resulting output is spendable by anyone, so following it would have
  produced a block whose reward was swept immediately.

### Fixed (platform)
- Windows workers use a per-thread current-thread runtime instead of building a
  full multi-thread runtime per OS thread.

### Fixed (tooling)
- `populate_strings_bible` no longer loops unconditionally. Its dedupe set was
  never populated, so the `while` condition could not change and the loop appended
  to `strings.txt` forever; the file is now the dedupe set, sampling is O(1), and
  a capped attempt count ends a pass that cannot make progress.

## [0.1.2] - 2025-08-10
- __Critical bug fix__ in mining algorithm

### Added
- Added .env tempate file to release

### Changed
- Fixed `hash_meets_target` function to correctly compare the block hash and the target in __big endian__ format matching Bitcoin Core’s proof-of-work check..
- Updated frequency of changing addresses
- Update RPC connection variable in linux

### Notes 
- __All previous releases are broken and obsolete__ and should not be used due to critical bug in mining that prevented ever finding valid blocks. Upgrade Immdiately.


## [0.1.1] - 2025-08-07

### Added
- **Payout address rotation support** via `payout_addresses.txt`
  - New `.env` field: `PAYOUT_ADDRESSES`
  - Automatically selects a new payout address every 30 seconds
  - Winning address is removed from the list after a block is found
  - Prevents address reuse and mimics Satoshi's block-by-block behavior
- **Compatibility with [bitcoin_mass_address_generator](https://crates.io/crates/bitcoin_mass_address_generator)**
  - Use generated HD wallets to populate your payout address file
- Support for multi-threaded mining with `NUM_TASKS` config
- Enhanced `.env` configurability and miner runtime diagnostics

### Changed
- Updated `README.md` with new configuration, examples, and address file format
- Improved internal logging and error handling during mining and block submission
- Minor CLI argument parsing improvements

### Notes
- This release focuses on privacy, automation, and education — helping solo miners experiment with rotating address strategies in line with Bitcoin best practices.
- While statistically unlikely to find a block on CPU, this version demonstrates fully valid block construction, hashing, and submission.

### Security
- Ensure that all payout addresses in `payout_addresses.txt` are valid `bc1q` SegWit addresses to avoid invalid block submissions.
