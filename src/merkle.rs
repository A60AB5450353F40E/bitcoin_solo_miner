use bitcoin::hashes::{Hash, sha256d};
use once_cell::sync::Lazy;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct CachedTemplate {
    /// Serialized template transactions, coinbase excluded, in the template's
    /// own order (CTOR) and never decoded.
    pub raw_txs: Arc<[Vec<u8>]>,
    /// Merkle branch for index 0 (the coinbase).
    pub merkle_branch: Vec<[u8; 32]>,
    /// Sum of the raw byte lengths of the template transactions (coinbase
    /// excluded), so the miner can predict the block size before hashing.
    pub raw_bytes: usize,
    /// Identity of the template this cache was built from. A worker must not
    /// mine unless these match the live template: coinbasevalue embeds the fees
    /// of this exact tx set, so a mismatch yields bad-cb-amount.
    pub height: u64,
    pub prev_blockhash: String,
    pub fingerprint: u64,
}

pub static LAST_TXS: Lazy<RwLock<Option<Arc<CachedTemplate>>>> = Lazy::new(|| RwLock::new(None));

pub fn precompute_merkle_branch(mut hashes: Vec<[u8; 32]>, mut index: usize) -> Vec<[u8; 32]> {
    let mut branch = Vec::new();
    while hashes.len() > 1 {
        if hashes.len() % 2 != 0 {
            hashes.push(*hashes.last().unwrap());
        }
        let sibling_index = if index % 2 == 0 { index + 1 } else { index - 1 };
        branch.push(hashes[sibling_index]);
        index /= 2;
        hashes = hashes
            .chunks(2)
            .map(|pair| {
                let mut concat = [0u8; 64];
                concat[..32].copy_from_slice(&pair[0]);
                concat[32..].copy_from_slice(&pair[1]);
                sha256d::Hash::hash(&concat).to_byte_array()
            })
            .collect();
    }
    branch
}

pub fn compute_root_from_branch(
    mut hash: [u8; 32],
    branch: &[[u8; 32]],
    mut index: usize,
) -> [u8; 32] {
    for sibling in branch {
        let mut concat = [0u8; 64];
        if index % 2 == 0 {
            concat[..32].copy_from_slice(&hash);
            concat[32..].copy_from_slice(sibling);
        } else {
            concat[..32].copy_from_slice(sibling);
            concat[32..].copy_from_slice(&hash);
        }
        hash = sha256d::Hash::hash(&concat).to_byte_array();
        index /= 2;
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive_root(leaves: &[[u8; 32]]) -> [u8; 32] {
        let mut level = leaves.to_vec();
        while level.len() > 1 {
            if level.len() % 2 == 1 {
                let last = *level.last().unwrap();
                level.push(last);
            }
            level = level
                .chunks(2)
                .map(|pair| {
                    let mut concat = [0u8; 64];
                    concat[..32].copy_from_slice(&pair[0]);
                    concat[32..].copy_from_slice(&pair[1]);
                    sha256d::Hash::hash(&concat).to_byte_array()
                })
                .collect();
        }
        level[0]
    }

    fn leaf(i: usize) -> [u8; 32] {
        let mut a = [0u8; 32];
        a[0] = i as u8;
        a[31] = (i as u8).wrapping_mul(7);
        a
    }

    /// Covers the 6-leaf case (5 template txs + coinbase) and every odd level:
    /// 6 -> 3 -> 2 -> 1.
    #[test]
    fn branch_matches_naive_root_for_all_tx_counts() {
        for n in 1..=70usize {
            let leaves: Vec<[u8; 32]> = (0..n).map(leaf).collect();
            let expect = naive_root(&leaves);
            let branch = precompute_merkle_branch(leaves.clone(), 0);
            let root = compute_root_from_branch(leaves[0], &branch, 0);
            assert_eq!(root, expect, "merkle mismatch at n={n}");
        }
    }
}
