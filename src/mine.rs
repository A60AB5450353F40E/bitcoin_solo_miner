use crate::block_template::check_target_time;
use crate::merkle::{LAST_TXS, compute_root_from_branch};
use crate::utils::{
    FALLBACK_MAX_BLOCK_BYTES, MIDSTATE_BYTES, MIN_DIFFICULTY_RULE, Midstate, MinerResult,
    TAIL_BYTES, TAIL_NONCE_OFFSET, TimestampInputs, bits_to_target, build_coinbase_tx,
    meets_target, payout_script, predicted_block_size, serialize_header_to_array, serialized_size,
};
use crate::{BLOCK_TARGET, BLOCKS_ACCEPTED, GLOBAL_TEMPLATE, PAYOUT_ADDRESS, SENTENCES, STOPPING};
use bitcoin::hashes::Hash;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct MinedBlock {
    pub header: bitcoin::block::Header,
    pub coinbase: bitcoin::Transaction,
    /// Serialized template transactions, coinbase excluded, in the template's
    /// own order (CTOR) and never decoded.
    pub raw_txs: Arc<[Vec<u8>]>,
    pub payout_address: String,
    pub height: u64,
    pub sizelimit: usize,
}

/// Total hashes attempted, reported by the stats ticker in main.
pub static HASH_COUNT: AtomicU64 = AtomicU64::new(0);

/// How the last header timestamp was chosen, for the stats ticker. Logging from
/// `mine_loop` itself would fire once per ~50 ms batch.
pub static LAST_TIMESTAMP_MODE: AtomicU8 = AtomicU8::new(0);

/// Nonces per coinbase/merkle build. Sized from the startup benchmark so one
/// batch is roughly BATCH_TARGET_MS of work.
static NONCE_BATCH: AtomicU32 = AtomicU32::new(1 << 16);

const BATCH_TARGET_MS: f64 = 50.0;
/// Used when there is nothing to mine: no template yet, or an exhausted payout
/// list, in which case every worker parks here rather than spinning.
const IDLE_SLEEP: Duration = Duration::from_secs(1);
/// How long a worker waits for the cache to catch up with a new template.
const COHERENCE_SLEEP: Duration = Duration::from_millis(5);

pub fn nonce_batch() -> u32 {
    NONCE_BATCH.load(Ordering::Relaxed)
}

/// Measures this thread's hash rate using the real hot path (midstate + tail +
/// target compare), not a bare sha256d loop.
pub fn bench_hashrate(sample_ms: u64) -> f64 {
    let header = bitcoin::block::Header {
        version: bitcoin::block::Version::from_consensus(0x2000_0000),
        prev_blockhash: bitcoin::BlockHash::all_zeros(),
        merkle_root: bitcoin::TxMerkleNode::all_zeros(),
        time: 0,
        bits: bitcoin::CompactTarget::from_consensus(0x1d00ffff),
        nonce: 0,
    };

    let bytes = serialize_header_to_array(&header);
    let prefix: [u8; MIDSTATE_BYTES] = bytes[..MIDSTATE_BYTES]
        .try_into()
        .expect("prefix is MIDSTATE_BYTES long");
    let midstate = Midstate::new(&prefix);
    let mut tail: [u8; TAIL_BYTES] = bytes[MIDSTATE_BYTES..]
        .try_into()
        .expect("tail is TAIL_BYTES long");

    // An unreachable target forces the full 32-byte compare every iteration,
    // which is what the mining loop does in the failure case.
    let unreachable = [0u8; 32];

    let start = Instant::now();
    let mut hashes: u64 = 0;
    let mut nonce: u32 = 0;
    loop {
        tail[TAIL_NONCE_OFFSET..].copy_from_slice(&nonce.to_le_bytes());
        let digest = midstate.hash(&tail);
        std::hint::black_box(meets_target(&digest, &unreachable));
        hashes += 1;
        nonce = nonce.wrapping_add(1);
        if hashes % 8192 == 0 && start.elapsed().as_millis() as u64 >= sample_ms {
            break;
        }
    }

    hashes as f64 / start.elapsed().as_secs_f64()
}

/// Picks a batch size worth ~50 ms so a worker cannot hold a stale header for
/// longer than that once a new template lands.
pub fn configure_batch(hashrate: f64) -> u32 {
    let n = (hashrate * BATCH_TARGET_MS / 1000.0) as u64;
    let n = n.clamp(1 << 12, 1 << 22) as u32;
    NONCE_BATCH.store(n, Ordering::Relaxed);
    n
}

pub fn pick_random_sentence() -> String {
    let guard = match SENTENCES.read() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.is_empty() {
        String::from("bch_solo_miner")
    } else {
        guard[fastrand::usize(..guard.len())].clone()
    }
}

struct MiningJob {
    header: bitcoin::block::Header,
    target: [u8; 32],
    coinbase: bitcoin::Transaction,
    raw_txs: Arc<[Vec<u8>]>,
    payout_address: String,
    height: u64,
    sizelimit: usize,
}

/// CPU-bound nonce search, on the blocking pool so it does not starve the
/// RPC/template tasks.
fn search_batch(mut job: MiningJob) -> Option<MinedBlock> {
    let batch = nonce_batch();
    let start_nonce = fastrand::u32(..);

    // Serialise and absorb the first 64 bytes once per batch instead of once
    // per nonce: only the last 4 bytes move from here on.
    let bytes = serialize_header_to_array(&job.header);
    let prefix: [u8; MIDSTATE_BYTES] = bytes[..MIDSTATE_BYTES]
        .try_into()
        .expect("prefix is MIDSTATE_BYTES long");
    let midstate = Midstate::new(&prefix);
    let mut tail: [u8; TAIL_BYTES] = bytes[MIDSTATE_BYTES..]
        .try_into()
        .expect("tail is TAIL_BYTES long");

    // One reference hash per batch: catches a slicing or byte-order mistake in
    // the fast path before it costs a block.
    debug_assert_eq!(
        midstate.hash(&tail),
        bitcoin::hashes::sha256d::Hash::hash(&bytes).to_byte_array(),
        "midstate path disagrees with a full sha256d"
    );

    for offset in 0..batch {
        let nonce = start_nonce.wrapping_add(offset);
        tail[TAIL_NONCE_OFFSET..].copy_from_slice(&nonce.to_le_bytes());
        let digest = midstate.hash(&tail);

        if meets_target(&digest, &job.target) {
            HASH_COUNT.fetch_add(offset as u64 + 1, Ordering::Relaxed);
            job.header.nonce = nonce;
            println!(
                "[pow] block found: height={} nonce={:#010x} merkle={}",
                job.height, job.header.nonce, job.header.merkle_root
            );
            return Some(MinedBlock {
                header: job.header,
                coinbase: job.coinbase,
                raw_txs: job.raw_txs,
                payout_address: job.payout_address,
                height: job.height,
                sizelimit: job.sizelimit,
            });
        }
    }

    HASH_COUNT.fetch_add(batch as u64, Ordering::Relaxed);
    None
}

pub async fn mine_loop() -> MinerResult<Option<MinedBlock>> {
    // The run's target was reached, so no new search starts. A worker already
    // in a batch finishes it and may still submit; `main` is about to return.
    if STOPPING.load(Ordering::Relaxed) {
        tokio::time::sleep(IDLE_SLEEP).await;
        return Ok(None);
    }

    let payout_address = match &*PAYOUT_ADDRESS.read().await {
        Some(addr) => addr.clone(),
        None => {
            // With a target set an empty list is fatal: no candidate can ever
            // be produced, so a wrapper would wait on RUN_DONE forever.
            if BLOCK_TARGET.get().is_some() {
                crate::fail(format!(
                    "no payout address with --count set, after {} accepted block(s); \
                     add addresses or set ADDRESS_MODE=loop",
                    BLOCKS_ACCEPTED.load(Ordering::Relaxed),
                ));
            }
            tokio::time::sleep(IDLE_SLEEP).await;
            return Ok(None);
        }
    };

    let (template, template_fp) = match &*GLOBAL_TEMPLATE.read().await {
        Some((t, fp)) => (Arc::clone(t), *fp),
        None => {
            tokio::time::sleep(IDLE_SLEEP).await;
            return Ok(None);
        }
    };

    let cached = match &*LAST_TXS.read().await {
        Some(c) => Arc::clone(c),
        None => {
            tokio::time::sleep(IDLE_SLEEP).await;
            return Ok(None);
        }
    };

    // The updater publishes the template only after rebuilding the cache. If
    // they still disagree (a rebuild raced a template swap), wait rather than
    // mine a header whose coinbasevalue we cannot honour.
    if cached.fingerprint != template_fp
        || cached.height != template.height
        || cached.prev_blockhash != template.previousblockhash
    {
        tokio::time::sleep(COHERENCE_SLEEP).await;
        return Ok(None);
    }

    let bits = u32::from_str_radix(&template.bits, 16)
        .map_err(|e| format!("template bits {:?}: {e}", template.bits))?;
    let target = match bits_to_target(bits) {
        Some(t) => t,
        None => {
            eprintln!(
                "[pow] template bits {:?} do not describe a usable target; skipping",
                template.bits
            );
            // Park instead of returning straight back into the loop: a template
            // that stays unusable would otherwise spin at 100% CPU.
            tokio::time::sleep(IDLE_SLEEP).await;
            return Ok(None);
        }
    };

    // Validated at startup by `addresses::init_addresses`, parsed here for the
    // script. `assume_checked` is deliberate: a testnet/regtest address carries a
    // version byte that matches no built-in Network variant.
    let payout_spk = payout_script(&payout_address)?;

    let aux_flags = match &template.coinbaseaux {
        Some(aux) if !aux.flags.is_empty() => hex::decode(&aux.flags)?,
        _ => Vec::new(),
    };

    let sentence = pick_random_sentence();

    // Single coinbase. No dummy pass, no wtxid tree, no commitment output.
    // `coinbasevalue` is used verbatim because every template transaction is in
    // this block: it is the maximum legal coinbase input, and we claim all of it.
    let coinbase_tx = build_coinbase_tx(
        template.height as u32,
        &aux_flags,
        sentence.as_bytes(),
        template.coinbasevalue,
        &payout_spk,
    )?;

    // Exact size check before any hashing: the only reason to skip a template is
    // that the assembled block would not fit, and that is known right now.
    let sizelimit = template
        .sizelimit
        .map(|v| v as usize)
        .unwrap_or(FALLBACK_MAX_BLOCK_BYTES);
    let coinbase_len = serialized_size(&coinbase_tx)?;
    let total_txs = cached.raw_txs.len() + 1;
    let predicted = predicted_block_size(coinbase_len, total_txs, cached.raw_bytes);
    if predicted > sizelimit {
        eprintln!(
            "[tpl] predicted block {predicted} B exceeds sizelimit {sizelimit} B; skipping template"
        );
        tokio::time::sleep(IDLE_SLEEP).await;
        return Ok(None);
    }

    let coinbase_txid = coinbase_tx.txid().to_byte_array();
    let merkle_root = compute_root_from_branch(coinbase_txid, &cached.merkle_branch, 0);

    // On a min-difficulty chain `bits` is a function of this block's own
    // timestamp, so the stamp is derived rather than taken from the clock
    // unclamped: see TimestampInputs.
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let (block_time, time_mode) = TimestampInputs {
        now,
        curtime: template.curtime,
        mintime: template.mintime.unwrap_or(0),
        parent_time: template.parent_time,
        target_time: template.target_time,
        target_time_conforms: check_target_time(template.target_time).is_none(),
        min_difficulty_rule: MIN_DIFFICULTY_RULE.load(Ordering::Relaxed),
    }
    .choose();
    LAST_TIMESTAMP_MODE.store(time_mode.code(), Ordering::Relaxed);

    let time = u32::try_from(block_time)
        .map_err(|_| format!("header timestamp {block_time} does not fit in u32"))?;

    let header = bitcoin::block::Header {
        version: bitcoin::block::Version::from_consensus(template.version),
        prev_blockhash: bitcoin::BlockHash::from_str(&template.previousblockhash)?,
        merkle_root: bitcoin::TxMerkleNode::from_slice(&merkle_root)?,
        time,
        bits: bitcoin::CompactTarget::from_consensus(bits),
        nonce: 0,
    };

    let job = MiningJob {
        header,
        target,
        coinbase: coinbase_tx,
        raw_txs: Arc::clone(&cached.raw_txs),
        payout_address,
        height: template.height,
        sizelimit,
    };

    let found = tokio::task::spawn_blocking(move || search_batch(job)).await?;
    Ok(found)
}
