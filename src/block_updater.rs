use crate::GLOBAL_TEMPLATE;
use crate::block_template::{BlockTemplate, check_target_time, fetch_parent_time};
use crate::fetch_block_template;
use crate::merkle::{CachedTemplate, LAST_TXS, precompute_merkle_branch};
use crate::utils::{
    MIN_DIFFICULTY_RULE, MinerResult, RpcTimeout, TARGET_TIME, min_difficulty_boundary,
};
use bitcoin::hashes::Hash;
use reqwest::Client;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// Fallback pace when longpoll is unavailable or has just failed.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Absolute ceiling on a longpoll hold. Must exceed `2 * tau` for the largest
/// conforming tau (600), or on a stalled tip the hold expires before the node
/// starts returning `powLimit` bits and the miner misses the whole window.
const LONGPOLL_TIMEOUT: Duration = Duration::from_secs(1_300);
/// After a longpoll failure, retry longpoll after this many fallback polls
/// instead of staying on 1 Hz polling for the life of the process.
const LONGPOLL_RETRY_POLLS: u32 = 30;
/// Minimum spacing between two longpoll issues. A hold that returns in a few
/// milliseconds was not a wakeup: it is a stale `longpollid`, an intermediary
/// RPC, or a proxy that does not keep the connection open. Re-issuing at once
/// would spin `getblocktemplate` (plus `getblockheader` on each tip change).
const MIN_LONGPOLL_GAP: Duration = Duration::from_millis(250);

/// Cheap identity for "same tip and same tx set". Hashing the raw hex is far
/// cheaper than decoding, and it is exact enough for a local consistency check.
///
/// `curtime` is deliberately excluded: a timestamp refresh must republish the
/// template without invalidating the transaction cache.
pub fn fingerprint(template: &BlockTemplate) -> u64 {
    let mut hasher = DefaultHasher::new();
    template.height.hash(&mut hasher);
    template.previousblockhash.hash(&mut hasher);
    template.bits.hash(&mut hasher);
    template.transactions.len().hash(&mut hasher);
    for tx in &template.transactions {
        tx.data.hash(&mut hasher);
    }
    hasher.finish()
}

/// `uint256` comparison, as the node performs it for CTOR: most significant
/// byte first, which is index 31 of a `to_byte_array()` (internal-order) value.
fn uint256_le(a: &[u8; 32], b: &[u8; 32]) -> bool {
    for i in (0..32).rev() {
        match a[i].cmp(&b[i]) {
            std::cmp::Ordering::Less => return true,
            std::cmp::Ordering::Greater => return false,
            std::cmp::Ordering::Equal => {}
        }
    }
    true
}

/// How long to keep the longpoll open before asking again.
///
/// On a min-difficulty chain `bits` is recomputed from `curtime` on every
/// request, so a template only starts carrying `powLimit` once `curtime` passes
/// `parent_time + 2 * tau`. The hold is therefore measured against that boundary
/// and the template's own `curtime`, not against the moment the request was
/// issued: a template fetched 90 s before the boundary would otherwise be held
/// 2 * tau seconds *past* it, and the miner would spend that window hashing a
/// harder target than the network requires.
fn longpoll_hold(tau: u64, parent_time: u64, curtime: u64, min_diff: bool) -> Duration {
    let ceiling = LONGPOLL_TIMEOUT.as_secs();

    let secs = if !min_diff {
        // `bits` does not depend on our timestamp, so the only reason to re-ask
        // is a stale `curtime`. Two target intervals is plenty.
        ceiling.min(tau.saturating_mul(2)).max(5)
    } else if parent_time == 0 {
        // Boundary unknown: fall back to the worst case for this cadence.
        ceiling.min(tau.saturating_mul(2).saturating_add(2)).max(5)
    } else {
        let boundary = min_difficulty_boundary(parent_time, tau);
        if curtime > boundary {
            // Already min-difficulty: nothing to chase, so hold normally.
            ceiling.min(tau.saturating_mul(2)).max(5)
        } else {
            // Expire just past the point where powLimit becomes available.
            ceiling
                .min(boundary.saturating_sub(curtime).saturating_add(2))
                .max(5)
        }
    };

    Duration::from_secs(secs)
}

/// Called synchronously from `main`; spawns the polling/longpoll task itself.
pub fn update_block(client: Arc<Client>, rpc_url: String, rpc_user: String, rpc_pass: String) {
    tokio::spawn(async move {
        let mut longpoll_tip: Option<String> = None;
        let mut longpoll_ok = true;
        let mut polls_since_failure: u32 = 0;
        let mut last_logged: Option<u64> = None;
        // (previousblockhash, its header time). One getblockheader per tip, not
        // per template: a curtime refresh re-delivers the same parent hash.
        let mut cached_parent: Option<(String, u64)> = None;
        // Timestamp of the template the current longpoll is holding for: the
        // hold is measured against the minimum-difficulty boundary, which is
        // expressed in terms of that template's own `curtime`.
        let mut held_curtime: u64 = 0;
        let mut held_parent_time: u64 = 0;

        loop {
            if !longpoll_ok {
                polls_since_failure = polls_since_failure.saturating_add(1);
                if polls_since_failure >= LONGPOLL_RETRY_POLLS {
                    longpoll_ok = true;
                    polls_since_failure = 0;
                    longpoll_tip = None;
                    println!("[tpl] retrying longpoll");
                }
            }

            let holding = longpoll_ok && longpoll_tip.is_some();
            let timeout = if holding {
                Some(longpoll_hold(
                    TARGET_TIME.load(Ordering::Relaxed),
                    held_parent_time,
                    held_curtime,
                    MIN_DIFFICULTY_RULE.load(Ordering::Relaxed),
                ))
            } else {
                None
            };

            let issued_at = Instant::now();
            let outcome = tokio::select! {
                r = fetch_block_template(
                    &client,
                    &rpc_url,
                    &rpc_user,
                    &rpc_pass,
                    longpoll_tip.as_deref(),
                    timeout,
                ) => r,
                _ = crate::REFRESH.notified() => {
                    // An accepted block: drop the longpoll and refetch at once.
                    longpoll_tip = None;
                    continue;
                }
            };

            match outcome {
                Ok(mut template) => {
                    if let Some(warning) = check_target_time(template.target_time) {
                        eprintln!("[tpl] WARNING: {warning}");
                    }
                    TARGET_TIME.store(template.target_time, Ordering::Relaxed);

                    match &cached_parent {
                        Some((h, t)) if h == &template.previousblockhash => {
                            template.parent_time = *t;
                        }
                        _ => {
                            match fetch_parent_time(
                                &client,
                                &rpc_url,
                                &rpc_user,
                                &rpc_pass,
                                &template.previousblockhash,
                            )
                            .await
                            {
                                Ok(Some(t)) => {
                                    template.parent_time = t;
                                    cached_parent = Some((template.previousblockhash.clone(), t));
                                }
                                Ok(None) => {}
                                Err(e) => eprintln!(
                                    "[tpl] parent header unavailable ({e}); timestamps pinned \
                                     to curtime"
                                ),
                            }
                        }
                    }

                    let fp = fingerprint(&template);
                    let tip = template.previousblockhash.clone();
                    let height = template.height;
                    let bits = template.bits.clone();
                    let n_tx = template.transactions.len();
                    let sizelimit = template.sizelimit;
                    let tau = template.target_time;
                    let parent_time = template.parent_time;
                    let curtime = template.curtime;
                    let min_diff_at = if parent_time == 0 {
                        "unknown".to_string()
                    } else {
                        min_difficulty_boundary(parent_time, tau).to_string()
                    };

                    let needs_rebuild = {
                        let guard = LAST_TXS.read().await;
                        match guard.as_ref() {
                            Some(cached) => cached.fingerprint != fp,
                            None => true,
                        }
                    };

                    let template_arc = Arc::new(template);

                    if needs_rebuild {
                        // Cache first, publish second. A published template with a
                        // stale tx cache pays fees for transactions it does not
                        // include, which is a bad-cb-amount rejection.
                        match update_tx_cache_once(&template_arc, fp).await {
                            Ok(()) => {
                                *GLOBAL_TEMPLATE.write().await = Some((template_arc, fp));
                                if last_logged != Some(fp) {
                                    println!(
                                        "[tpl] height={height} prev={tip} bits={bits} tau={tau}s \
                                         txs={n_tx} sizelimit={sizelimit:?} \
                                         min_diff_at={min_diff_at}"
                                    );
                                    last_logged = Some(fp);
                                }
                            }
                            Err(e) => {
                                eprintln!("[tpl] discarding template: {e}");
                                *GLOBAL_TEMPLATE.write().await = None;
                                *LAST_TXS.write().await = None;
                                last_logged = None;
                            }
                        }
                    } else {
                        *GLOBAL_TEMPLATE.write().await = Some((template_arc, fp));
                    }

                    held_curtime = curtime;
                    held_parent_time = parent_time;
                    longpoll_tip = Some(tip);
                }
                Err(e) => {
                    if e.downcast_ref::<RpcTimeout>().is_some() {
                        if holding {
                            println!("[tpl] longpoll hold expired; re-issuing");
                        }
                    } else if holding {
                        eprintln!(
                            "[tpl] longpoll failed ({e}); falling back to {POLL_INTERVAL:?} \
                             polling, longpoll retried in {LONGPOLL_RETRY_POLLS} polls"
                        );
                        longpoll_ok = false;
                        polls_since_failure = 0;
                        longpoll_tip = None;
                    } else {
                        eprintln!("[tpl] getblocktemplate failed: {e}");
                    }
                }
            }

            // Longpoll steady state needs no sleep: the request itself blocks
            // until something changes, and a real wakeup spent longer than
            // MIN_LONGPOLL_GAP blocked, so the branch below costs nothing in
            // the normal case. Anything faster than that is a request loop.
            if !holding {
                tokio::select! {
                    _ = sleep(POLL_INTERVAL) => {}
                    _ = crate::REFRESH.notified() => { longpoll_tip = None; }
                }
            } else if let Some(gap) = MIN_LONGPOLL_GAP.checked_sub(issued_at.elapsed()) {
                sleep(gap).await;
            }
        }
    });
}

pub async fn update_tx_cache_once(template: &BlockTemplate, fingerprint: u64) -> MinerResult<()> {
    let mut raw_txs: Vec<Vec<u8>> = Vec::with_capacity(template.transactions.len());
    let mut txids: Vec<[u8; 32]> = Vec::with_capacity(template.transactions.len());
    let mut raw_bytes = 0usize;

    // Order is the template's, verbatim. BCH enforces canonical transaction
    // ordering (CTOR, November 2018): every transaction after the coinbase must
    // be sorted by txid, so the node's order is the only valid one and any
    // reordering makes the block invalid.
    //
    // That is also why `depends` must not be used to sort here. CTOR removed the
    // topological requirement precisely so a transaction may spend an output
    // created later in the same block, which means a child can legitimately
    // precede its parent; reordering to satisfy `depends` would break CTOR.
    //
    // Every template transaction is included, never trimmed. That is what makes
    // `coinbasevalue` (the maximum allowable coinbase input) exactly right.
    //
    // Txids come from the template and the bytes stay bytes: rust-bitcoin's
    // consensus parser does not understand the BCH CashTokens output prefix, so
    // decoding a token transaction fails, and a round-trip through a type we do
    // not model is a risk with no upside.
    for (i, tx) in template.transactions.iter().enumerate() {
        let txid = tx
            .txid
            .as_deref()
            .ok_or_else(|| format!("template transaction {i} has no txid"))?;
        txids.push(txid.parse::<bitcoin::Txid>()?.to_byte_array());

        let raw = hex::decode(&tx.data)
            .map_err(|e| format!("template transaction {i} is not hex: {e}"))?;
        raw_bytes += raw.len();
        raw_txs.push(raw);
    }

    // Guard against a future edit reintroducing a sort. The node's test is a
    // `uint256` comparison, not a `memcmp` of the internal-order array: walking
    // from index 0 compares the *least* significant bytes first and fires on
    // almost every real template.
    debug_assert!(
        txids.windows(2).all(|w| uint256_le(&w[0], &w[1])),
        "template transactions are not in canonical (CTOR) order"
    );

    // Slot 0 is reserved for the coinbase, whose txid is unknown until mine_loop
    // picks an extranonce. A branch for index 0 needs [0u8; 32] there.
    let leaves: Vec<[u8; 32]> = std::iter::once([0u8; 32]).chain(txids).collect();
    let merkle_branch = precompute_merkle_branch(leaves, 0);

    *LAST_TXS.write().await = Some(Arc::new(CachedTemplate {
        raw_txs: raw_txs.into(),
        merkle_branch,
        raw_bytes,
        height: template.height,
        prev_blockhash: template.previousblockhash.clone(),
        fingerprint,
    }));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Display order -> internal order, the way `Txid::to_byte_array()` hands
    /// it back and the way the node's `uint256` stores it.
    fn internal(display: [u8; 32]) -> [u8; 32] {
        let mut a = display;
        a.reverse();
        a
    }

    #[test]
    fn ctor_compare_starts_at_the_most_significant_byte() {
        // Highest byte only: display 0x01…00 is larger than display 0x00…ff.
        // A memcmp from internal index 0 compares the least significant bytes
        // first and calls the larger number smaller, which is the bug this
        // guards against.
        let mut high = [0u8; 32];
        let mut low = [0u8; 32];
        high[0] = 0x01;
        low[31] = 0xff;

        assert!(!uint256_le(&internal(high), &internal(low)));
        assert!(uint256_le(&internal(low), &internal(high)));

        // Equal top byte, differing in the middle: catches a comparator that
        // looks only at the extremes and so passes the case above by accident.
        let mut mid_hi = [0u8; 32];
        let mut mid_lo = [0u8; 32];
        mid_hi[0] = 0x05;
        mid_hi[16] = 0x02;
        mid_lo[0] = 0x05;
        mid_lo[16] = 0x01;

        assert!(!uint256_le(&internal(mid_hi), &internal(mid_lo)));
        assert!(uint256_le(&internal(mid_lo), &internal(mid_hi)));

        // Equal is `<=`, matching the node's `uint256` comparison.
        assert!(uint256_le(&internal(high), &internal(high)));
    }
}
