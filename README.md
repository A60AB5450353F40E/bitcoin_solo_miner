# Bitcoin Cash CPU Solo Miner

A CPU-based Bitcoin Cash solo miner, forked from [viraladmin/bitcoin_solo_miner](https://github.com/viraladmin/bitcoin_solo_miner), a Bitcoin CPU miner, and adapted to BCH. Educational, experimental and intended for BCH testnet use: it builds and submits real blocks rather than joining a pool or using ASIC hardware.

Instead of only iterating the block header's `nonce`, it inserts a random sentence from a text file into the coinbase transaction's scriptSig as extra nonce data. That changes the coinbase txid, hence the Merkle root, so every attempt produces a unique header. The text lives inside the coinbase input, exactly like traditional extra nonce data — not in an OP_RETURN output.

The odds of finding a block on a CPU remain astronomically low. What is true is that every hash has the same probability of falling under the target as one from any other machine; ASICs simply make far more attempts per second.

---

## Requirements

- A Bitcoin Cash node (BCHN v29+ tested) with RPC enabled and fully synced to the tip.
- `target_time` is required in the template on purpose. Testnet, tempnet and
  regtest return it and apply the minimum-difficulty rule, so its absence there
  is a misconfiguration worth failing on. Against a node that does not return
  the field the miner logs the missing-field error and never hashes, by design
  rather than by oversight.
- A payout address in the legacy base58 encoding (`1…` P2PKH or `3…` P2SH).
- RPC credentials in `.env`.

The miner refuses to start against a non-BCH daemon, and refuses to start while the node is in initial block download.

---

## Configuration

Configuration is read from `.env`, in the working directory or beside the binary (`DOTENV_PATH` overrides both):

```env
RPC_URL="http://127.0.0.1:8332"
RPC_USER="admin"
RPC_PASS="pass"
PAYOUT_ADDRESSES="addresses.txt"
STRINGS_FILE="strings.txt"
ADDRESS_MODE="loop"
ADDRESSES_STATE="addresses.txt.state"
NUM_TASKS=20
MIN_DIFFICULTY_RULE="auto"
```

| Variable | Meaning |
|---|---|
| `RPC_URL` | URL of the node's RPC endpoint |
| `RPC_USER` / `RPC_PASS` | RPC credentials (`bitcoin.conf` must use `rpcuser`/`rpcpassword`; rpcauth-only or cookie auth is not supported) |
| `PAYOUT_ADDRESSES` | File with one payout address per line. Read-only: the miner never writes to it |
| `STRINGS_FILE` | UTF-8 file with one sentence per line (truncated to 80 bytes, leaving room for the height push and aux flags inside the 100-byte scriptSig cap) |
| `ADDRESS_MODE` | `once` (default), `loop`, or `random` |
| `ADDRESSES_STATE` | Cursor file; defaults to `<PAYOUT_ADDRESSES>.state` |
| `NUM_TASKS` | Concurrent mining workers, clamped to the CPUs this process can actually use |
| `MIN_DIFFICULTY_RULE` | `auto` (default), `on`, or `off`. Whether `nBits` is a function of the block's own timestamp, which is what pins the header time below the minimum-difficulty threshold. `auto` is on for every chain except mainnet |

---

## Payout addresses

The address file is only ever read. The cursor lives in the state file, which records the file's length and sha256 so a restart can tell "the operator appended addresses" (cursor preserved) from "the operator replaced the list" (cursor reset to the first entry). It is written to a temporary file, `fsync`ed, then renamed, so a crash mid-write leaves the previous cursor rather than a truncated one.

Advancing happens **only when a block is accepted**, not on a timer. The sentence already re-randomises the Merkle root every attempt, so rotating the payout address on a clock adds no proof-of-work entropy and only costs you the guarantee that a payout script is never reused.

- `once` — walk the list, then stop. Once every address has been used, `PAYOUT_ADDRESS` becomes `None`, workers idle, and the log says so. Add addresses to the file (cursor preserved) or delete the state file to start over.
- `loop` — walk the list and wrap around.
- `random` — pick a different entry for each block.

A `submitblock` result of `duplicate` does **not** advance the cursor. The node already had the block; it may be this miner's own earlier submission whose response was lost, in which case the address is already on chain, or it may be someone else's block. The two cannot be told apart, so the cursor is held rather than risk skipping an address that was never used.

### Address format

Legacy base58 (`1…`, `3…`) is byte-identical on Bitcoin and BCH, so it works unchanged. Nothing verifies that the address matches the node's chain: a mainnet `1…` address on tempnet or regtest parses into a valid P2PKH script and the block is accepted, sending the reward to an address whose key you may not control on that chain. Check `[rpc] … chain=` at startup against `addresses.txt`. Rejected on purpose:

- `bc1…` / `tb1…` — bech32 is a Bitcoin (SegWit) encoding that does not exist on BCH. The resulting `OP_0 <20 bytes>` output would be **spendable by anyone**, so such lines are skipped with a warning.
- `bitcoincash:…` — cashaddr needs a real base32+polymod decoder, which is not implemented yet. Use the legacy encoding of the same key.

A line may instead be a raw scriptPubKey in hex, prefixed `0x`, used verbatim as the coinbase output script:

```
0x76a914<20-byte hash160 in hex>88ac
```

Nothing is decoded and nothing is checked — not the node's chain, not the script type — so the miner cannot tell whether the resulting output is spendable. A typo produces a block that pays an unspendable script, or one anyone can spend; either way the reward is gone. A bare `0x` is accepted and produces an empty script, which is also anyone-can-spend. The payload is capped at the consensus `MAX_SCRIPT_SIZE` of 10,000 bytes.

---

## Running

```
./bch_solo_miner
```

There are no command-line flags. In the background:

```
nohup ./bch_solo_miner > miner.log 2>&1 &
```

Startup prints the node version and chain, whether the minimum-difficulty rule is in force, the parsed address count with its mode, cursor and the address currently in use, a per-thread hash-rate benchmark, and the nonce batch size derived from it.

### Template handling

- `getblocktemplate` is called with `capabilities: ["coinbasevalue", "longpoll"]`. Longpoll uses the current tip hash as `longpollid`, so a new block wakes the miner immediately instead of leaving it up to a second behind. A failed longpoll falls back to one-second polling and is retried after a bounded number of polls rather than disabling itself for the life of the process.
- Each hold is measured against the template's own minimum-difficulty boundary: when `curtime` is still short of `parent_time + 2 * target_time` the request expires two seconds past it, so `powLimit` bits are picked up as soon as the node offers them. Otherwise the hold is two target intervals, under an absolute ceiling. A submission drops the hold immediately.
- Every transaction in the template is included. The miner never trims the tx set, so `coinbasevalue` (the maximum allowable coinbase input) is used verbatim and the block can never be rejected for claiming fees it does not include.
- Transactions are kept as the template's own serialized bytes, in the template's own order, and never decoded. rust-bitcoin's consensus parser cannot represent BCH CashTokens outputs, and the Merkle root and block body only need txids and bytes. The order must not be touched: BCH enforces canonical transaction ordering (CTOR), so anything other than the node's txid-sorted order is rejected, and `depends` is informational rather than a sorting key.
- `coinbaseaux.flags` from the template are embedded in the coinbase scriptSig between the height push and the extra nonce, and they count against the same 100-byte budget as the sentence.
- Before hashing, the exact assembled block size is compared against the template's `sizelimit`. The assembled length is also asserted equal to the prediction, so a varint, header-size or counting mistake fails before submission instead of at the node. Nothing assumes a 200 kB or 1 MB limit; whatever the node advertises is used.
- The transaction cache is rebuilt before a template is published, so workers never mine a header whose `coinbasevalue` refers to a different tx set.
- `target_time` is the consensus target block time for the block being **built**, so at a 600 → 60 change it is already 60 for the first short block while the tip that block extends was still a 600-second block. Reading the tip's cadence instead would place the minimum-difficulty threshold one block early.
- On a chain with the minimum-difficulty rule, `nBits` is a function of the block's own timestamp: a block is eligible for minimum difficulty only when its time exceeds the parent's by more than `2 * target_time`. The node computed the template's `bits` against `curtime`, so the header time is held at `parent_time + 2 * target_time` rather than allowed to drift past it; drifting would demand `powLimit` bits from a header carrying the DAA's, which the node rejects as `bad-diffbits`. The parent's own timestamp is read with `getblockheader` (not `mintime`, which is a median and can be minutes older), one call per tip.

---

## Stats output

```
[stats] 92.39 MH/s bench=11.83 MH/s cumulative=8337677608 height=Some(335307) tau=60s find~2.2m waste~0.39% min_diff=in 33s time=now payout_set=true
```

`MH/s` is the aggregate across all workers while `bench` is the startup
single-thread figure, so on a multi-core box the two are not comparable
directly; `cumulative` is the lifetime hash count.

| Field | Meaning |
|---|---|
| `bench` | Per-thread hash rate measured at startup, used to size the nonce batch |
| `tau` | Target block time for the template being mined |
| `find~` | This miner's expected time to find a block at the template's difficulty |
| `waste~` | Share of work lost to polling lag (mean 0.5 s ÷ `find~`), an upper bound once longpoll is working |
| `min_diff` | `eligible`, `in <time>`, `unknown`, or `n/a` when the rule does not apply |
| `time` | How the last header timestamp was chosen: `now`, `pinned` at the threshold, or `template` when the parent time is unavailable |

`find~` follows difficulty, so it shortens sharply at a cadence boundary where difficulty drops. Block spacing on such a chain is not an error signal: with the minimum-difficulty rule, a stalled tip makes every miner's next block easy, and the cadence settles again once blocks arrive on time.

---

## Notes and limitations

- Educational use only; not competitive with ASIC hardware.
- Intended for testnet use, including tempo nets like tempnet. The timestamp logic is built around the minimum-difficulty rule, which is why `target_time` is required rather than optional.
- CPU hash rates are extremely low, so finding a block is very unlikely, but possible.
- The hash-rate gain assumes SHA-NI (x86) or the SHA2 extension (aarch64).
  `sha2` 0.10 selects these at runtime; without them the miner works but is much
  slower than the "twice as fast" claim in the release notes. `[bench]` at
  startup reports what this machine actually does.
- Mining requires a fully synced node: templates come from the node, including `bits`, `coinbasevalue` and `sizelimit`. The miner never recomputes difficulty.
- Payout scripts are P2PKH, P2SH, or a raw `0x` scriptPubKey. The `0x` form is not checked for spendability, and the addresses shipped in `addresses.txt` are anyone-can-spend on purpose, as a testnet fixture.
- Every address in the shipped `addresses.txt` is anyone-can-spend on purpose,
  as a testnet fixture: `0x` locking script is spendable with `0x51` unlocking
  script and `0x51` with `0x`, and the P2SH address's redeem script is `0x51`,
  i.e. `OP_1`. Replace them before mining anywhere the coins have value.
- Nothing in the RPC surface exposes `fPowAllowMinDifficultyBlocks`, so `auto` infers it from the chain name. If the node does not report the chain as `main` yet runs without the rule (or vice versa), set `MIN_DIFFICULTY_RULE` explicitly.
- Timestamps come from the local clock, so a drifting system clock still matters for the non-pinned case; keep NTP running.
- `NUM_TASKS` above the usable CPU count is clamped, and SHA-NI is latency-bound, so a small oversubscription can occasionally win on some–core boxes; measure rather than assume.
- `edition = "2024"` requires Rust 1.85 or newer.
- Full cashaddr support is a known remaining gap.

---

## License

MIT License — free to use, modify, and share.

---

## Disclaimer

Provided as is, for educational and research purposes only. No warranty for any damages incurred by using these tools.
