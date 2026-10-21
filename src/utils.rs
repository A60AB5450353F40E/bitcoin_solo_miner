use crate::SENTENCES;

// `bitcoin::script::Builder` only exists from 0.32; on 0.31 this is the path.
use bitcoin::blockdata::script::Builder;
use bitcoin::consensus::{Encodable, encode::VarInt};
use bitcoin::{
    Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
    absolute::LockTime,
    address::{Address, AddressType},
    block::Header as BlockHeader,
    hashes::Hash,
    transaction::Version,
};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::digest::generic_array::GenericArray;
use std::cmp::Ordering;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::time::Duration;

pub type MinerResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Used only when a template omits `sizelimit`. BCH mainnet advertises ~32 MB;
/// the real limit always comes from the template, because a testnet can be much
/// smaller than this.
pub const FALLBACK_MAX_BLOCK_BYTES: usize = 32_000_000;

/// Consensus bounds on the coinbase scriptSig.
const MIN_CB_SCRIPT: usize = 2;
const MAX_CB_SCRIPT: usize = 100;

// ---------------------------------------------------------------------------
// Chain cadence / timestamp policy state
// ---------------------------------------------------------------------------

/// Consensus target block time for the block currently being mined, in seconds.
/// 600 until the first template arrives. Feeds cadence decisions and the stats
/// line only: `bits` always comes from the node.
pub static TARGET_TIME: AtomicU64 = AtomicU64::new(600);

/// Whether this chain applies the minimum-difficulty rule, i.e. whether `bits`
/// depends on the block's own timestamp. Set once by `preflight`.
pub static MIN_DIFFICULTY_RULE: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// JSON-RPC
// ---------------------------------------------------------------------------

/// Marker error for a request that hit its per-request timeout (longpoll holds
/// expire this way, and that is not a failure).
#[derive(Debug)]
pub struct RpcTimeout;

impl fmt::Display for RpcTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("rpc request timed out")
    }
}

impl std::error::Error for RpcTimeout {}

/// Body preview for an error message. Must not slice by byte index: an error
/// page with a multi-byte character straddling the cut would panic inside the
/// RPC layer, turning a diagnosable failure into an abort.
fn log_preview(body: &str) -> String {
    body.chars().take(200).collect()
}

/// Single entry point for every ordinary RPC call.
pub async fn rpc_call(
    client: &Client,
    rpc_url: &str,
    rpc_user: &str,
    rpc_pass: &str,
    method: &str,
    params: Value,
) -> MinerResult<Value> {
    rpc_call_timeout(client, rpc_url, rpc_user, rpc_pass, method, params, None).await
}

/// Same, with an optional per-request timeout that overrides the client default
/// (needed for longpoll holds).
///
/// Nodes report failures as HTTP 200 with an `error` object, and auth failures
/// as a 401 with an empty body, so both are handled here instead of leaking into
/// callers as "EOF while parsing".
pub async fn rpc_call_timeout(
    client: &Client,
    rpc_url: &str,
    rpc_user: &str,
    rpc_pass: &str,
    method: &str,
    params: Value,
    timeout: Option<Duration>,
) -> MinerResult<Value> {
    let payload = json!({
        "jsonrpc": "1.0",
        "id": "bch_solo_miner",
        "method": method,
        "params": params,
    });

    let mut request = client
        .post(rpc_url)
        .basic_auth(rpc_user, Some(rpc_pass))
        .json(&payload);
    if let Some(t) = timeout {
        request = request.timeout(t);
    }

    let res = match request.send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => return Err(Box::new(RpcTimeout)),
        Err(e) => return Err(Box::new(e)),
    };

    let status = res.status();
    match status {
        StatusCode::UNAUTHORIZED => {
            return Err(format!(
                "401 from {rpc_url} for user {rpc_user:?}: check RPC_USER/RPC_PASS, \
                 and that bitcoin.conf has matching rpcuser/rpcpassword \
                 (rpcauth-only or cookie auth will always 401 here)"
            )
            .into());
        }
        StatusCode::FORBIDDEN => {
            return Err(format!(
                "403 from {rpc_url}: check rpcallowip / rpcbind for this interface"
            )
            .into());
        }
        _ => {}
    }

    let body = res.text().await?;
    let parsed: Value = serde_json::from_str(&body).map_err(|e| {
        format!(
            "non-JSON reply from {rpc_url} (HTTP {status}): {e}; body: {:?}",
            log_preview(&body)
        )
    })?;

    if let Some(err) = parsed.get("error").filter(|e| !e.is_null()) {
        return Err(format!("{method}: {err}").into());
    }

    Ok(parsed.get("result").cloned().unwrap_or(Value::Null))
}

#[derive(Debug, Deserialize)]
struct NetworkInfo {
    subversion: String,
}

#[derive(Debug, Deserialize)]
struct ChainInfo {
    chain: String,
    blocks: u64,
    headers: u64,
    initialblockdownload: bool,
}

/// Refuse to mine against the wrong daemon. Cheap insurance: a Bitcoin Core
/// node bound to the same port produces templates this miner cannot serialize.
pub async fn preflight(
    client: &Client,
    rpc_url: &str,
    rpc_user: &str,
    rpc_pass: &str,
) -> MinerResult<()> {
    let net: NetworkInfo = serde_json::from_value(
        rpc_call(
            client,
            rpc_url,
            rpc_user,
            rpc_pass,
            "getnetworkinfo",
            json!([]),
        )
        .await?,
    )?;

    let is_bch = [
        "Bitcoin Cash Node",
        "Bitcoin ABC",
        "bchd",
        "Bitcoin Unlimited",
    ]
    .iter()
    .any(|name| net.subversion.contains(name));
    if !is_bch {
        return Err(format!(
            "refusing to mine: {rpc_url} is not a BCH node ({})",
            net.subversion
        )
        .into());
    }

    let chain: ChainInfo = serde_json::from_value(
        rpc_call(
            client,
            rpc_url,
            rpc_user,
            rpc_pass,
            "getblockchaininfo",
            json!([]),
        )
        .await?,
    )?;

    if chain.initialblockdownload {
        return Err(format!(
            "node is in IBD ({}/{} headers); getblocktemplate would hand back a stale tip",
            chain.blocks, chain.headers
        )
        .into());
    }

    // Mainnet has no minimum-difficulty rule, so `bits` does not depend on the
    // block's own timestamp and `now` is the better stamp. Every other chain
    // BCHN ships depends on it. Nothing in the RPC surface exposes
    // fPowAllowMinDifficultyBlocks, hence the chain-name heuristic plus an
    // override for a future network that changes mainnet's targeting.
    //
    // Bias toward "on": wrongly pinning costs a stale-but-valid timestamp,
    // wrongly not pinning costs the block (bad-diffbits).
    let rule = match dotenvy::var("MIN_DIFFICULTY_RULE") {
        Err(_) => chain.chain != "main",
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => chain.chain != "main",
            "on" | "1" | "true" | "yes" => true,
            "off" | "0" | "false" | "no" => false,
            other => {
                return Err(
                    format!("MIN_DIFFICULTY_RULE={other:?} is not one of auto|on|off").into(),
                );
            }
        },
    };
    MIN_DIFFICULTY_RULE.store(rule, AtomicOrdering::Relaxed);

    println!(
        "[rpc] {} chain={} height={} node={} min_difficulty_rule={rule}",
        rpc_url, chain.chain, chain.blocks, net.subversion
    );
    if chain.chain != "main" {
        println!("[rpc] NOTE: non-mainnet chain; verify payout scripts match this network");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Extranonce corpus
// ---------------------------------------------------------------------------

fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

pub fn load_sentences_from_file(path: &str) -> MinerResult<()> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;

    let mut lines: Vec<String> = content
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();

    if lines.is_empty() {
        return Err(format!("{path} contains no non-empty lines").into());
    }

    // The whole coinbase scriptSig is capped at 100 bytes; the BIP34 height push
    // is 4 bytes at current heights. Keep sentences at 80 so aux flags and the
    // height push always fit without silent truncation in the hot path.
    for line in &mut lines {
        truncate_utf8(line, 80);
    }

    let mut guard = SENTENCES.write().map_err(|_| "SENTENCES lock poisoned")?;
    *guard = lines;
    Ok(())
}

// ---------------------------------------------------------------------------
// Payout
// ---------------------------------------------------------------------------

/// Turns a payout address into a script, refusing anything that is not valid on
/// BCH. `bc1…` is a Bitcoin SegWit encoding: on BCH the resulting `OP_0 <20b>`
/// output is anyone-can-spend, so a block paying it would be swept instantly.
pub fn payout_script(address: &str) -> MinerResult<ScriptBuf> {
    let addr = address.trim();
    let lower = addr.to_ascii_lowercase();

    if lower.starts_with("bc1") || lower.starts_with("tb1") || lower.starts_with("bcrt1") {
        return Err(format!(
            "{addr}: bech32 is a Bitcoin (SegWit) encoding, not a BCH address; that \
             output would be spendable by anyone. Use legacy 1…/3…"
        )
        .into());
    }
    if lower.starts_with("bitcoincash:")
        || lower.starts_with("bchtest:")
        || lower.starts_with("bchreg:")
    {
        return Err(format!(
            "{addr}: cashaddr is not supported by this build yet; use the legacy \
             base58 encoding of the same key"
        )
        .into());
    }

    let unchecked = addr
        .parse::<Address<bitcoin::address::NetworkUnchecked>>()
        .map_err(|e| format!("{addr}: {e}"))?;
    let checked = unchecked.assume_checked();

    match checked.address_type() {
        Some(AddressType::P2pkh | AddressType::P2sh) => {}
        other => {
            return Err(format!(
                "{addr}: unsupported payout script ({other:?}); need P2PKH 1… or P2SH 3…"
            )
            .into());
        }
    }

    Ok(checked.script_pubkey())
}

// ---------------------------------------------------------------------------
// Coinbase
// ---------------------------------------------------------------------------

/// BCH coinbase: scriptSig = BIP34 height push || coinbaseaux.flags || extranonce.
/// No witness commitment output, empty witness stack, single payout output.
pub fn build_coinbase_tx(
    height: u32,
    aux_flags: &[u8],
    extra: &[u8],
    reward: u64,
    payout_spk: &ScriptBuf,
) -> MinerResult<Transaction> {
    // push_int emits OP_0 / OP_1..OP_16 / minimal CScriptNum. At height 334148
    // this is `03 44 19 05`, which satisfies BIP34's check.
    let mut script_sig = Builder::new()
        .push_int(height as i64)
        .into_script()
        .into_bytes();

    // Raw append, not push_slice: the coinbase scriptSig is never executed, only
    // its length and leading height push are checked, and push_slice would add a
    // PUSHDATA opcode (and a stray OP_0 for empty input).
    script_sig.extend_from_slice(aux_flags);

    let budget = MAX_CB_SCRIPT.saturating_sub(script_sig.len());
    if extra.len() > budget {
        script_sig.extend_from_slice(&extra[..budget]);
    } else {
        script_sig.extend_from_slice(extra);
    }

    while script_sig.len() < MIN_CB_SCRIPT {
        script_sig.push(0x00);
    }

    if script_sig.len() > MAX_CB_SCRIPT {
        return Err(format!("coinbase scriptSig len {} > 100", script_sig.len()).into());
    }

    Ok(Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: script_sig.into(),
            sequence: Sequence::MAX,
            witness: Witness::new(), // BCH: always empty
        }],
        output: vec![TxOut {
            value: Amount::from_sat(reward),
            script_pubkey: payout_spk.clone(),
        }],
    })
}

/// Legacy serialized size of a transaction. Used instead of `total_size()` so the
/// number is literally the bytes that will go into the block.
pub fn serialized_size(tx: &Transaction) -> MinerResult<usize> {
    let mut buf = Vec::with_capacity(256);
    tx.consensus_encode(&mut buf)?;
    Ok(buf.len())
}

/// Exact block size before hashing: 80-byte header, tx-count varint, coinbase,
/// and the template transactions we cached byte-for-byte.
///
/// `total_txs` counts the coinbase, i.e. it is the number the varint encodes.
/// Passing the template count alone would under-count by one and pick a 1-byte
/// varint for a 253-transaction block.
pub fn predicted_block_size(
    coinbase_len: usize,
    total_txs: usize,
    template_raw_bytes: usize,
) -> usize {
    let count_varint = if total_txs < 0xfd {
        1
    } else if total_txs <= 0xffff {
        3
    } else {
        5
    };
    80 + count_varint + coinbase_len + template_raw_bytes
}

// ---------------------------------------------------------------------------
// Target / header
// ---------------------------------------------------------------------------

/// Expands compact `bits` into a 32-byte big-endian target.
///
/// `None` means the compact value cannot describe a usable target (zero
/// mantissa, or an exponent past the end of a 256-bit number). Callers must skip
/// the template rather than mine against a made-up target.
pub fn bits_to_target(bits: u32) -> Option<[u8; 32]> {
    let exponent = ((bits >> 24) & 0xff) as usize;
    let mantissa = bits & 0x007f_ffff;
    if mantissa == 0 || exponent == 0 {
        return None;
    }

    let mut target = [0u8; 32];
    if exponent <= 3 {
        let m = mantissa >> (8 * (3 - exponent));
        target[29] = ((m >> 16) & 0xff) as u8;
        target[30] = ((m >> 8) & 0xff) as u8;
        target[31] = (m & 0xff) as u8;
    } else if exponent <= 32 {
        let start = 32 - exponent;
        target[start] = ((mantissa >> 16) & 0xff) as u8;
        if start + 1 < 32 {
            target[start + 1] = ((mantissa >> 8) & 0xff) as u8;
        }
        if start + 2 < 32 {
            target[start + 2] = (mantissa & 0xff) as u8;
        }
    } else {
        return None;
    }
    Some(target)
}

pub fn serialize_header_to_array(header: &BlockHeader) -> [u8; 80] {
    let mut buf = [0u8; 80];
    let mut i = 0;

    buf[i..i + 4].copy_from_slice(&header.version.to_consensus().to_le_bytes());
    i += 4;
    buf[i..i + 32].copy_from_slice(header.prev_blockhash.as_byte_array());
    i += 32;
    buf[i..i + 32].copy_from_slice(header.merkle_root.as_byte_array());
    i += 32;
    buf[i..i + 4].copy_from_slice(&header.time.to_le_bytes());
    i += 4;
    buf[i..i + 4].copy_from_slice(&header.bits.to_consensus().to_le_bytes());
    i += 4;
    buf[i..i + 4].copy_from_slice(&header.nonce.to_le_bytes());
    i += 4;

    debug_assert_eq!(i, 80);
    buf
}

// ---------------------------------------------------------------------------
// Header hashing / PoW
// ---------------------------------------------------------------------------

/// Header bytes absorbed into the reusable state: version, previous block hash,
/// and the first 28 bytes of the merkle root. The split lands inside the merkle
/// root because 64 is SHA256's block boundary, not a field boundary.
pub const MIDSTATE_BYTES: usize = 64;

/// The remaining header bytes: last 4 merkle bytes, time, bits, nonce.
pub const TAIL_BYTES: usize = 16;

/// Offset of the nonce inside the tail: merkle remainder 4 + time 4 + bits 4.
pub const TAIL_NONCE_OFFSET: usize = 12;

/// SHA256 initial hash value.
const SHA256_IV: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// Precomputed SHA256 state for an 80-byte header.
///
/// A fresh sha256d over 80 bytes costs three compressions plus the padding
/// machinery for each of the two hashes. Bytes 0..64 of the header never change
/// within a batch, so block 1 is compressed once and both padding blocks are
/// built once, leaving exactly two compressions per nonce and no hasher
/// construction or clone in the hot path.
pub struct Midstate {
    /// SHA256 state after absorbing header bytes 0..64.
    state: [u32; 8],
    /// Header block 2 with its padding and the 640-bit total length already in
    /// place. Only bytes 0..16 differ per attempt.
    block2: [u8; 64],
    /// The outer hash's second half: 0x80, zeros, and the 256-bit length. Its
    /// first 32 bytes are the inner digest, written per attempt.
    outer: [u8; 64],
}

impl Midstate {
    pub fn new(header_prefix: &[u8; MIDSTATE_BYTES]) -> Self {
        // Block 1 is exactly 64 bytes, so it needs no padding of its own.
        let mut state = SHA256_IV;
        sha2::compress256(&mut state, &[GenericArray::clone_from_slice(header_prefix)]);

        // Block 2 holds the 16 tail bytes, then 0x80, then the length of the
        // whole 80-byte message in bits at the very end of the block.
        let mut block2 = [0u8; 64];
        block2[TAIL_BYTES] = 0x80;
        block2[56..64].copy_from_slice(&(80u64 * 8).to_be_bytes());

        // The outer hash's input is the 32-byte inner digest: 0x80 follows it
        // immediately, then the 256-bit length.
        let mut outer = [0u8; 64];
        outer[32] = 0x80;
        outer[56..64].copy_from_slice(&(32u64 * 8).to_be_bytes());

        Self {
            state,
            block2,
            outer,
        }
    }

    /// Absorbs header bytes 64..80 and returns sha256d in natural (internal)
    /// byte order: the order `sha256d::Hash::to_byte_array()` returns, and the
    /// reverse of the order anything human-facing displays.
    #[inline]
    pub fn hash(&self, tail: &[u8; TAIL_BYTES]) -> [u8; 32] {
        let mut block2 = self.block2;
        block2[..TAIL_BYTES].copy_from_slice(tail);

        let mut state = self.state;
        sha2::compress256(&mut state, &[GenericArray::clone_from_slice(&block2)]);

        let mut outer = self.outer;
        for (i, word) in state.iter().enumerate() {
            outer[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }

        let mut final_state = SHA256_IV;
        sha2::compress256(&mut final_state, &[GenericArray::clone_from_slice(&outer)]);

        let mut digest = [0u8; 32];
        for (i, word) in final_state.iter().enumerate() {
            digest[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        digest
    }
}

/// `digest` is natural order, `target` is display-order big-endian as
/// `bits_to_target` produces it.
///
/// Walks the digest from its most significant end and the target from its first
/// byte, so no 32-byte copy is needed and the usual failure — hash nowhere near
/// the target — decides on the very first comparison.
#[inline]
pub fn meets_target(digest: &[u8; 32], target: &[u8; 32]) -> bool {
    for i in (0..32).rev() {
        match digest[i].cmp(&target[31 - i]) {
            Ordering::Less => return true,
            Ordering::Greater => return false,
            Ordering::Equal => {}
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Minimum-difficulty aware header timestamp
// ---------------------------------------------------------------------------

/// A block is eligible for minimum difficulty when its timestamp is strictly
/// greater than this. `parent_time + 2 * target_time` is 1,200 s at the
/// 600-tick schedule and 120 s at the 60-tick schedule; it is derived from the
/// schedule rather than held constant.
pub fn min_difficulty_boundary(parent_time: u64, target_time: u64) -> u64 {
    // Both operands come off the wire. The node's own expression is checked
    // arithmetic over an int64; ours must not wrap either.
    parent_time.saturating_add(target_time.saturating_mul(2))
}

/// Inputs to the header-timestamp decision, in seconds since the epoch.
///
/// A struct rather than six positional arguments: `curtime` and `parent_time`
/// are one transposition away from each other and would be invisible in review.
#[derive(Debug, Clone, Copy)]
pub struct TimestampInputs {
    /// Local wall clock.
    pub now: u64,
    /// Template `curtime`: the timestamp the node computed `bits` against, and
    /// therefore guaranteed to be on the correct side of the flip point.
    pub curtime: u64,
    /// Template `mintime` (parent median time past + 1); 0 when absent.
    pub mintime: u64,
    /// Parent block timestamp; 0 when unknown.
    pub parent_time: u64,
    /// Consensus target block time for this block.
    pub target_time: u64,
    /// Whether `target_time` passed the tick-schedule conformance check.
    ///
    /// Deliberately gates the ceiling, not just logging: an over-estimated tau
    /// lets us stamp a time the node considers eligible, which is bad-diffbits.
    /// An under-estimated one is merely stale. So when the field is not
    /// conforming we fall back to `curtime`, which is valid by construction.
    pub target_time_conforms: bool,
    /// Whether `bits` depends on the block's own timestamp.
    pub min_difficulty_rule: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampMode {
    /// Unclamped: `max(now, curtime, mintime)`.
    Now,
    /// Held at the minimum-difficulty boundary, because going past it would want
    /// `powLimit` bits while the template carries the DAA's.
    Pinned,
    /// Parent time or cadence unknown, so only `curtime` is safe.
    TemplateTime,
}

impl TimestampMode {
    pub const fn code(self) -> u8 {
        match self {
            TimestampMode::Now => 0,
            TimestampMode::Pinned => 1,
            TimestampMode::TemplateTime => 2,
        }
    }

    pub fn name(code: u8) -> &'static str {
        match code {
            0 => "now",
            1 => "pinned",
            _ => "template",
        }
    }
}

impl TimestampInputs {
    pub fn choose(&self) -> (u64, TimestampMode) {
        // BCHN sets curtime = max(parent MTP + 1, now), so it already clears
        // mintime; the max() is defensive and cannot raise the floor above
        // curtime in practice.
        let floor = self.curtime.max(self.mintime);

        if !self.min_difficulty_rule {
            return (self.now.max(floor), TimestampMode::Now);
        }
        if self.parent_time == 0 || !self.target_time_conforms {
            return (floor, TimestampMode::TemplateTime);
        }

        let boundary = min_difficulty_boundary(self.parent_time, self.target_time);

        // BCHN: eligible iff `time > parent_time + 2 * spacing`, strictly. The
        // boundary itself is therefore NOT eligible and is the latest timestamp
        // that keeps the template's non-min-difficulty bits valid.
        //
        // If the node already computed bits for an eligible curtime (floor >
        // boundary: a stalled chain, which is precisely when min difficulty
        // matters), every later timestamp remains eligible and there is no
        // ceiling.
        let ceiling = if floor > boundary { u64::MAX } else { boundary };

        // min-then-max, so a floor sitting exactly on the boundary wins over
        // the ceiling instead of dropping below mintime.
        let time = self.now.min(ceiling).max(floor);
        let mode = if time < self.now {
            TimestampMode::Pinned
        } else {
            TimestampMode::Now
        };
        (time, mode)
    }
}

// ---------------------------------------------------------------------------
// Block assembly / submission
// ---------------------------------------------------------------------------

/// Assembles header || varint count || coinbase || verbatim template bytes.
///
/// Template transactions are never decoded: rust-bitcoin's consensus parser
/// does not understand the BCH CashTokens output prefix, and a round-trip
/// through a type we do not fully model is a correctness risk with no upside.
pub fn build_full_block(
    header: BlockHeader,
    coinbase: &Transaction,
    raw_txs: &[Vec<u8>],
    sizelimit: usize,
) -> MinerResult<Vec<u8>> {
    let coinbase_len = serialized_size(coinbase)?;
    let raw_bytes: usize = raw_txs.iter().map(|t| t.len()).sum();
    let total_txs = raw_txs.len() + 1;
    let predicted = predicted_block_size(coinbase_len, total_txs, raw_bytes);

    let mut buf = Vec::with_capacity(predicted);
    header.consensus_encode(&mut buf)?;
    VarInt(total_txs as u64).consensus_encode(&mut buf)?;
    coinbase.consensus_encode(&mut buf)?;
    for raw in raw_txs {
        buf.extend_from_slice(raw);
    }

    // Catches a varint rule, header size or counting mistake on the one path
    // that can lose a block, for microseconds.
    if buf.len() != predicted {
        return Err(format!(
            "assembled block is {} bytes but predicted {predicted}",
            buf.len()
        )
        .into());
    }
    if buf.len() > sizelimit {
        return Err(format!(
            "assembled block is {} bytes, over the template sizelimit of {sizelimit}",
            buf.len()
        )
        .into());
    }
    Ok(buf)
}

#[derive(Debug, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// JSON null: the node accepted the block.
    Accepted,
    /// `"duplicate"`: the node already had it. Not proof that it was ours.
    Duplicate,
}

pub async fn submit_block(
    client: &Client,
    rpc_url: &str,
    rpc_user: &str,
    rpc_pass: &str,
    block_bytes: Vec<u8>,
) -> MinerResult<SubmitOutcome> {
    let result = rpc_call(
        client,
        rpc_url,
        rpc_user,
        rpc_pass,
        "submitblock",
        json!([hex::encode(&block_bytes)]),
    )
    .await?;

    // Success is JSON null. A rejection is a STRING in `result`, not an error.
    match &result {
        Value::Null => Ok(SubmitOutcome::Accepted),
        Value::String(s) if s == "duplicate" => Ok(SubmitOutcome::Duplicate),
        other => Err(format!("submitblock rejected: {other}").into()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::{Hash, sha256d};

    #[test]
    fn bits_expand_matches_template_target() {
        // From getblocktemplate: bits 1c4aec55 -> 000000004aec5500..00
        let target = bits_to_target(0x1c4aec55).unwrap();
        assert_eq!(&target[..7], &[0x00, 0x00, 0x00, 0x00, 0x4a, 0xec, 0x55]);
        assert!(target[7..].iter().all(|b| *b == 0));

        // Small exponents shift instead of indexing.
        assert_eq!(bits_to_target(0x01003456).unwrap()[31], 0x00);
        assert_eq!(bits_to_target(0x02008000).unwrap()[31], 0x80);
        assert_eq!(bits_to_target(0x03008000).unwrap()[30], 0x80);

        // Unusable compact values are refused, not guessed at.
        assert!(bits_to_target(0x00000000).is_none());
        assert!(bits_to_target(0x1d000000).is_none());
        assert!(bits_to_target(0x2100ffff).is_none()); // exponent 33 > 32
    }

    fn display_to_internal(be: [u8; 32]) -> [u8; 32] {
        let mut internal = be;
        internal.reverse();
        internal
    }

    #[test]
    fn meets_target_compares_in_display_order() {
        let target = bits_to_target(0x1c4aec55).unwrap();

        // Digest whose display form equals the target exactly.
        assert!(meets_target(&display_to_internal(target), &target));

        // One above the target.
        let mut above = target;
        above[6] = above[6].wrapping_add(1);
        assert!(!meets_target(&display_to_internal(above), &target));

        // One below the target.
        let mut below = target;
        below[6] = below[6].wrapping_sub(1);
        assert!(meets_target(&display_to_internal(below), &target));

        // Extremes.
        assert!(meets_target(&[0u8; 32], &target));
        assert!(!meets_target(&[0xffu8; 32], &target));
    }

    #[test]
    fn midstate_matches_full_sha256d() {
        let mut header = BlockHeader {
            version: bitcoin::block::Version::from_consensus(0x2000_0000),
            prev_blockhash: bitcoin::BlockHash::all_zeros(),
            merkle_root: bitcoin::TxMerkleNode::all_zeros(),
            time: 1_700_000_000,
            bits: bitcoin::CompactTarget::from_consensus(0x1d00ffff),
            nonce: 0,
        };

        for nonce in [0u32, 1, 0xdead_beef, 0xffff_ffff] {
            header.nonce = nonce;
            let full = serialize_header_to_array(&header);
            let want = sha256d::Hash::hash(&full).to_byte_array();

            let prefix: [u8; MIDSTATE_BYTES] = full[..MIDSTATE_BYTES].try_into().unwrap();
            let tail: [u8; TAIL_BYTES] = full[MIDSTATE_BYTES..].try_into().unwrap();
            let got = Midstate::new(&prefix).hash(&tail);

            assert_eq!(got, want, "nonce {nonce:#010x}");
        }
    }

    #[test]
    fn header_is_exactly_eighty_bytes() {
        let header = BlockHeader {
            version: bitcoin::block::Version::from_consensus(0x2000_0000),
            prev_blockhash: bitcoin::BlockHash::all_zeros(),
            merkle_root: bitcoin::TxMerkleNode::all_zeros(),
            time: 1,
            bits: bitcoin::CompactTarget::from_consensus(0x1d00ffff),
            nonce: 2,
        };
        assert_eq!(serialize_header_to_array(&header).len(), 80);
        assert_eq!(MIDSTATE_BYTES + TAIL_BYTES, 80);
        assert_eq!(MIDSTATE_BYTES + TAIL_NONCE_OFFSET + 4, 80);
    }

    #[test]
    fn bech32_payout_is_refused() {
        // Would be a P2WPKH script: anyone-can-spend on BCH.
        assert!(payout_script("bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4").is_err());
        assert!(payout_script("BITCOINCASH:qq07w4p0zue5p8f2n9sd0rph6ax0uwj5zs0xy0u4r").is_err());
    }

    #[test]
    fn legacy_payout_is_accepted() {
        // Legacy base58 is byte-identical on BCH and Bitcoin.
        assert!(payout_script("1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2").is_ok());
    }

    #[test]
    fn predicted_size_counts_header_and_varint() {
        // 1 tx (just a coinbase): 80 header + 1 varint + coinbase.
        assert_eq!(predicted_block_size(100, 1, 0), 181);
        // 300 txs total: 3-byte varint.
        assert_eq!(predicted_block_size(100, 300, 1_000), 80 + 3 + 100 + 1_000);
        // Exactly at the boundary: 0xfd txs needs the extended form.
        assert_eq!(predicted_block_size(100, 0xfd, 0), 80 + 3 + 100);
    }

    fn tempnet(now: u64, curtime: u64, mintime: u64) -> TimestampInputs {
        TimestampInputs {
            now,
            curtime,
            mintime,
            parent_time: 1_000,
            target_time: 60,
            target_time_conforms: true,
            min_difficulty_rule: true,
        }
    }

    /// Literal transcription of the tempnet rule:
    /// `pblock->GetBlockTime() > pindexPrev->GetBlockTime() + 2 * tick_delta_next`.
    fn node_eligible(time: u64, parent_time: u64, target_time: u64) -> bool {
        time > parent_time + 2 * target_time
    }

    #[test]
    fn mainnet_never_pins() {
        let i = TimestampInputs {
            min_difficulty_rule: false,
            now: 9_999,
            ..tempnet(0, 1_000, 1_000)
        };
        assert_eq!(i.choose(), (9_999, TimestampMode::Now));
    }

    #[test]
    fn tempnet_pins_at_the_boundary_not_below_it() {
        // parent 1_000 + 2 * 60 = 1_120. BCHN's `>` makes 1_120 non-eligible,
        // so it is the latest safe stamp; 1_121 would demand powLimit bits while
        // our header still carries the DAA's -> bad-diffbits.
        assert_eq!(
            tempnet(1_120, 1_005, 1_000).choose(),
            (1_120, TimestampMode::Now)
        );
        assert_eq!(
            tempnet(1_121, 1_005, 1_000).choose(),
            (1_120, TimestampMode::Pinned)
        );
        assert_eq!(
            tempnet(9_999, 1_005, 1_000).choose(),
            (1_120, TimestampMode::Pinned)
        );
    }

    #[test]
    fn already_eligible_timestamps_stay_fresh() {
        // curtime past the flip point: the node gave us powLimit bits, so any
        // later timestamp is still consistent. The stalled-chain case, where a
        // block takes an hour to find.
        assert_eq!(
            tempnet(9_999, 1_200, 1_000).choose(),
            (9_999, TimestampMode::Now)
        );
    }

    #[test]
    fn floor_on_the_boundary_does_not_undercut_mintime() {
        // curtime == boundary is not eligible, and we must not drop below
        // mintime to avoid it.
        assert_eq!(tempnet(9_999, 1_120, 1_120).choose().0, 1_120);
    }

    #[test]
    fn unknown_parent_falls_back_to_curtime() {
        let i = TimestampInputs {
            parent_time: 0,
            ..tempnet(9_999, 1_005, 1_000)
        };
        assert_eq!(i.choose(), (1_005, TimestampMode::TemplateTime));
    }

    #[test]
    fn non_conforming_tau_never_widens_the_ceiling() {
        // tau=7 does not divide 600. Stamping now would risk landing past the
        // node's real boundary; curtime is always safe.
        let i = TimestampInputs {
            target_time: 7,
            target_time_conforms: false,
            ..tempnet(9_999, 1_005, 1_000)
        };
        assert_eq!(i.choose(), (1_005, TimestampMode::TemplateTime));
    }

    #[test]
    fn chosen_timestamp_preserves_the_nodes_verdict() {
        for curtime in [1_000u64, 1_005, 1_119, 1_120, 1_121, 2_000] {
            let i = tempnet(9_999, curtime, 1_000);
            let (time, _) = i.choose();

            assert_eq!(
                node_eligible(time, 1_000, 60),
                node_eligible(curtime, 1_000, 60),
                "curtime={curtime}: pin changed the eligibility verdict"
            );
            assert!(time >= curtime, "curtime={curtime}");
            assert!(time >= i.mintime, "curtime={curtime}");
        }
    }
}
