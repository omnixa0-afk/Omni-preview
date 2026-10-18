// The hash tree of a file's content (docs/decisions/attachment-content-tree.md).
//
// A file is cut into 1 MiB leaves (the transfer chunk), the leaves are hashed, and the hashes are
// joined into a Merkle tree the way RFC 6962 does it: a range of n leaves splits at the largest power
// of two below n, so the tree needs no padding and every aligned run of 16 leaves (a block) is a node
// of it. A leaf hashes as `SHA-256(0x00 ‖ data)`, a node as `SHA-256(0x01 ‖ left ‖ right)` — apart, so a
// node cannot pass for a leaf. The content's identity is `SHA-256(0x02 ‖ size ‖ tree root)`: the size
// is part of it, so the same tree under another length is another content.
//
// What travels: the root, in the offer. A block comes with the hashes of its leaves (to check each
// chunk as it lands) and the path from the block up to the root (to check those hashes) — about
// twenty hashes, so nothing has to be fetched before the first byte is checked. The sender keeps the
// layer of block hashes (32 bytes per 16 MiB) and rebuilds lower nodes from the data it reads anyway.
//
// NB: `include!`d into lib.rs.

/// A leaf: the attachment chunk. Fixed, so the identity does not depend on how a transfer is cut.
pub const CONTENT_TREE_LEAF_BYTES: u64 = 1 << 20;
/// Leaves per block — the unit a source hands over with one proof. A power of two, so a block is a
/// node of the tree.
pub const CONTENT_TREE_BLOCK_LEAVES: u64 = 16;
pub const CONTENT_TREE_BLOCK_BYTES: u64 = CONTENT_TREE_LEAF_BYTES * CONTENT_TREE_BLOCK_LEAVES;

pub type ContentHash = [u8; 32];

const CONTENT_TREE_LEAF_TAG: u8 = 0x00;
const CONTENT_TREE_NODE_TAG: u8 = 0x01;
const CONTENT_TREE_ROOT_TAG: u8 = 0x02;

pub fn content_leaf_hash(data: &[u8]) -> ContentHash {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update([CONTENT_TREE_LEAF_TAG]);
    hasher.update(data);
    hasher.finalize().into()
}

pub fn content_node_hash(left: &ContentHash, right: &ContentHash) -> ContentHash {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update([CONTENT_TREE_NODE_TAG]);
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

/// The identity of `size` bytes whose tree root is `tree_root`.
pub fn content_id(size: u64, tree_root: &ContentHash) -> ContentHash {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update([CONTENT_TREE_ROOT_TAG]);
    hasher.update(size.to_le_bytes());
    hasher.update(tree_root);
    hasher.finalize().into()
}

/// Leaves of `size` bytes: an empty file still has one (empty) leaf.
pub fn content_leaf_count(size: u64) -> u64 {
    size.div_ceil(CONTENT_TREE_LEAF_BYTES).max(1)
}

pub fn content_block_count(size: u64) -> u64 {
    content_leaf_count(size).div_ceil(CONTENT_TREE_BLOCK_LEAVES)
}

/// The leaves of block `index`: `first..first + count`.
pub fn content_block_leaves(size: u64, index: u64) -> (u64, u64) {
    let first = index * CONTENT_TREE_BLOCK_LEAVES;
    let count = content_leaf_count(size).saturating_sub(first).min(CONTENT_TREE_BLOCK_LEAVES);
    (first, count)
}

/// The largest power of two strictly below `n` (n ≥ 2) — where RFC 6962 splits a range.
fn content_tree_split(n: usize) -> usize {
    debug_assert!(n >= 2);
    1 << (usize::BITS - 1 - (n - 1).leading_zeros())
}

/// The root over `hashes` (leaves, or nodes of one level such as the block layer). `None` for none.
pub fn content_tree_root(hashes: &[ContentHash]) -> Option<ContentHash> {
    match hashes.len() {
        0 => None,
        1 => Some(hashes[0]),
        n => {
            let k = content_tree_split(n);
            Some(content_node_hash(&content_tree_root(&hashes[..k])?, &content_tree_root(&hashes[k..])?))
        }
    }
}

/// The path from `hashes[index]` up to the root: sibling hashes, lowest first.
pub fn content_tree_proof(hashes: &[ContentHash], index: usize) -> Option<Vec<ContentHash>> {
    if index >= hashes.len() {
        return None;
    }
    let mut path = Vec::new();
    content_tree_proof_into(hashes, index, &mut path)?;
    Some(path)
}

fn content_tree_proof_into(hashes: &[ContentHash], index: usize, path: &mut Vec<ContentHash>) -> Option<()> {
    let n = hashes.len();
    if n == 1 {
        return Some(());
    }
    let k = content_tree_split(n);
    if index < k {
        content_tree_proof_into(&hashes[..k], index, path)?;
        path.push(content_tree_root(&hashes[k..])?);
    } else {
        content_tree_proof_into(&hashes[k..], index - k, path)?;
        path.push(content_tree_root(&hashes[..k])?);
    }
    Some(())
}

/// The root that `hash` at `index` among `count` proves with `path`, or `None` when the path does not
/// fit the shape (too short, too long, index out of range).
pub fn content_tree_root_from_proof(
    index: u64,
    count: u64,
    hash: &ContentHash,
    path: &[ContentHash],
) -> Option<ContentHash> {
    if index >= count {
        return None;
    }
    if count == 1 {
        return path.is_empty().then_some(*hash);
    }
    let (sibling, below) = path.split_last()?;
    let k = content_tree_split(usize::try_from(count).ok()?) as u64;
    if index < k {
        let left = content_tree_root_from_proof(index, k, hash, below)?;
        Some(content_node_hash(&left, sibling))
    } else {
        let right = content_tree_root_from_proof(index - k, count - k, hash, below)?;
        Some(content_node_hash(sibling, &right))
    }
}

/// Whether block `index` of the content `id` (of `size` bytes) has the leaves `leaf_hashes`: they
/// make the block's node, `path` lifts it to the tree root, and size + root make `id`.
pub fn verify_content_block(
    id: &ContentHash,
    size: u64,
    index: u64,
    leaf_hashes: &[ContentHash],
    path: &[ContentHash],
) -> bool {
    let (_, expected_leaves) = content_block_leaves(size, index);
    if expected_leaves == 0 || leaf_hashes.len() as u64 != expected_leaves {
        return false;
    }
    let Some(block) = content_tree_root(leaf_hashes) else {
        return false;
    };
    content_tree_root_from_proof(index, content_block_count(size), &block, path)
        .is_some_and(|tree_root| &content_id(size, &tree_root) == id)
}

/// What the sender keeps of a content's tree: the block layer (one hash per 16 MiB) and the identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentTreeLayer {
    pub size: u64,
    pub blocks: Vec<ContentHash>,
    pub id: ContentHash,
}

impl ContentTreeLayer {
    pub fn from_blocks(size: u64, blocks: Vec<ContentHash>) -> Option<Self> {
        if blocks.len() as u64 != content_block_count(size) {
            return None;
        }
        let id = content_id(size, &content_tree_root(&blocks)?);
        Some(Self { size, blocks, id })
    }

    /// The path from block `index` to the root.
    pub fn block_proof(&self, index: u64) -> Option<Vec<ContentHash>> {
        content_tree_proof(&self.blocks, usize::try_from(index).ok()?)
    }
}

/// Builds the tree as the bytes go by, in one pass and in constant memory apart from the block layer.
pub struct ContentTreeBuilder {
    size: u64,
    leaf: sha2::Sha256,
    leaf_fill: u64,
    leaves: Vec<ContentHash>,
    blocks: Vec<ContentHash>,
}

impl Default for ContentTreeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ContentTreeBuilder {
    pub fn new() -> Self {
        use sha2::Digest as _;
        let mut leaf = sha2::Sha256::new();
        leaf.update([CONTENT_TREE_LEAF_TAG]);
        Self {
            size: 0,
            leaf,
            leaf_fill: 0,
            leaves: Vec::with_capacity(CONTENT_TREE_BLOCK_LEAVES as usize),
            blocks: Vec::new(),
        }
    }

    pub fn update(&mut self, mut bytes: &[u8]) {
        use sha2::Digest as _;
        while !bytes.is_empty() {
            let room = (CONTENT_TREE_LEAF_BYTES - self.leaf_fill) as usize;
            let take = room.min(bytes.len());
            self.leaf.update(&bytes[..take]);
            self.leaf_fill += take as u64;
            self.size += take as u64;
            bytes = &bytes[take..];
            if self.leaf_fill == CONTENT_TREE_LEAF_BYTES {
                self.close_leaf();
            }
        }
    }

    fn close_leaf(&mut self) {
        use sha2::Digest as _;
        let mut fresh = sha2::Sha256::new();
        fresh.update([CONTENT_TREE_LEAF_TAG]);
        let done = std::mem::replace(&mut self.leaf, fresh);
        self.leaves.push(done.finalize().into());
        self.leaf_fill = 0;
        if self.leaves.len() as u64 == CONTENT_TREE_BLOCK_LEAVES {
            self.close_block();
        }
    }

    fn close_block(&mut self) {
        if let Some(block) = content_tree_root(&self.leaves) {
            self.blocks.push(block);
        }
        self.leaves.clear();
    }

    /// Picks up a reading cut short: `blocks` are the first blocks of the content, read before. The
    /// next byte given is the first of block `blocks.len()`.
    pub fn resume(blocks: Vec<ContentHash>) -> Self {
        let mut builder = Self::new();
        builder.size = blocks.len() as u64 * CONTENT_TREE_BLOCK_BYTES;
        builder.blocks = blocks;
        builder
    }

    /// The blocks read whole so far — where a reading cut short picks up (`resume`).
    pub fn blocks_done(&self) -> &[ContentHash] {
        &self.blocks
    }

    /// Bytes taken so far.
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn finish(mut self) -> ContentTreeLayer {
        // A partial last leaf — or the one empty leaf of an empty file.
        if self.leaf_fill > 0 || self.size == 0 {
            self.close_leaf();
        }
        if !self.leaves.is_empty() {
            self.close_block();
        }
        let size = self.size;
        ContentTreeLayer::from_blocks(size, self.blocks)
            .expect("the builder closes exactly the blocks of what it was given")
    }
}

#[cfg(test)]
mod content_tree_tests {
    use super::*;

    fn fake(n: usize) -> Vec<ContentHash> {
        (0..n).map(|i| content_leaf_hash(&(i as u64).to_le_bytes())).collect()
    }

    #[test]
    fn the_root_over_blocks_is_the_root_over_leaves() {
        // Every aligned run of 16 leaves is a node, so the sender may keep only the block layer.
        for n in 1..=80 {
            let leaves = fake(n);
            let blocks: Vec<ContentHash> = leaves
                .chunks(CONTENT_TREE_BLOCK_LEAVES as usize)
                .map(|block| content_tree_root(block).unwrap())
                .collect();
            assert_eq!(content_tree_root(&leaves), content_tree_root(&blocks), "{n} leaves");
        }
    }

    #[test]
    fn a_reading_picked_up_at_a_block_gives_the_tree_of_one_reading() {
        let bytes: Vec<u8> = (0..(2 * CONTENT_TREE_BLOCK_BYTES + 12_345) as usize).map(|at| (at * 31 % 251) as u8).collect();
        let mut whole = ContentTreeBuilder::new();
        whole.update(&bytes);
        let whole = whole.finish();
        // Cut short mid-block: what is kept is the blocks read whole, and the reading resumes after them.
        let mut first = ContentTreeBuilder::new();
        first.update(&bytes[..(CONTENT_TREE_BLOCK_BYTES + 3 * CONTENT_TREE_LEAF_BYTES + 7) as usize]);
        let kept = first.blocks_done().to_vec();
        assert_eq!(kept.len(), 1);
        let mut resumed = ContentTreeBuilder::resume(kept);
        assert_eq!(resumed.size(), CONTENT_TREE_BLOCK_BYTES);
        resumed.update(&bytes[CONTENT_TREE_BLOCK_BYTES as usize..]);
        assert_eq!(resumed.finish(), whole);
    }

    #[test]
    fn every_proof_lifts_its_hash_to_the_root_and_nothing_else_does() {
        for n in 1..=40_usize {
            let hashes = fake(n);
            let root = content_tree_root(&hashes).unwrap();
            for i in 0..n {
                let path = content_tree_proof(&hashes, i).unwrap();
                assert_eq!(content_tree_root_from_proof(i as u64, n as u64, &hashes[i], &path), Some(root));
                // Another hash, another place, another count, a cut or a padded path: not the root.
                let other = content_leaf_hash(b"other");
                assert_ne!(content_tree_root_from_proof(i as u64, n as u64, &other, &path), Some(root));
                if n > 1 {
                    let j = (i + 1) % n;
                    assert_ne!(content_tree_root_from_proof(j as u64, n as u64, &hashes[i], &path), Some(root));
                    assert_ne!(content_tree_root_from_proof(i as u64, n as u64, &hashes[i], &path[1..]), Some(root));
                }
                let mut padded = path.clone();
                padded.push(root);
                assert_ne!(content_tree_root_from_proof(i as u64, n as u64, &hashes[i], &padded), Some(root));
            }
            assert_eq!(content_tree_root_from_proof(n as u64, n as u64, &hashes[0], &[]), None);
        }
    }

    #[test]
    fn a_node_cannot_pass_for_a_leaf() {
        let (a, b) = (content_leaf_hash(b"a"), content_leaf_hash(b"b"));
        let mut joined = Vec::new();
        joined.extend_from_slice(&a);
        joined.extend_from_slice(&b);
        assert_ne!(content_leaf_hash(&joined), content_node_hash(&a, &b));
    }

    #[test]
    fn the_size_is_part_of_the_identity() {
        let root = content_leaf_hash(b"x");
        assert_ne!(content_id(1, &root), content_id(2, &root));
    }

    fn bytes(len: usize) -> Vec<u8> {
        // Not periodic: two blocks of equal bytes would be equal nodes, and «the wrong block» would
        // pass for the right one.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn direct(data: &[u8]) -> ContentTreeLayer {
        let leaves: Vec<ContentHash> = if data.is_empty() {
            vec![content_leaf_hash(&[])]
        } else {
            data.chunks(CONTENT_TREE_LEAF_BYTES as usize).map(content_leaf_hash).collect()
        };
        let blocks = leaves
            .chunks(CONTENT_TREE_BLOCK_LEAVES as usize)
            .map(|block| content_tree_root(block).unwrap())
            .collect();
        ContentTreeLayer::from_blocks(data.len() as u64, blocks).unwrap()
    }

    #[test]
    fn the_builder_gives_the_tree_however_the_bytes_are_fed() {
        let leaf = CONTENT_TREE_LEAF_BYTES as usize;
        let block = CONTENT_TREE_BLOCK_BYTES as usize;
        for len in [0, 1, leaf - 1, leaf, leaf + 1, block, block + 1, 2 * block + 3 * leaf + 7] {
            let data = bytes(len);
            let expected = direct(&data);
            for piece in [1_000_usize, 65_536, leaf, 3 * leaf + 5] {
                let mut builder = ContentTreeBuilder::new();
                for part in data.chunks(piece) {
                    builder.update(part);
                }
                assert_eq!(builder.finish(), expected, "len {len}, fed by {piece}");
            }
        }
    }

    #[test]
    fn a_block_checks_against_the_identity_alone() {
        let leaf = CONTENT_TREE_LEAF_BYTES as usize;
        let data = bytes(2 * CONTENT_TREE_BLOCK_BYTES as usize + 3 * leaf + 7);
        let layer = direct(&data);
        let size = data.len() as u64;
        assert_eq!(content_block_count(size), 3);
        for index in 0..content_block_count(size) {
            let (first, count) = content_block_leaves(size, index);
            let leaves: Vec<ContentHash> = data
                .chunks(leaf)
                .skip(first as usize)
                .take(count as usize)
                .map(content_leaf_hash)
                .collect();
            let path = layer.block_proof(index).unwrap();
            assert!(verify_content_block(&layer.id, size, index, &leaves, &path), "block {index}");
            // A changed chunk, a missing leaf, or the wrong block: refused.
            let mut bad = leaves.clone();
            bad[0] = content_leaf_hash(b"tampered");
            assert!(!verify_content_block(&layer.id, size, index, &bad, &path));
            assert!(!verify_content_block(&layer.id, size, index, &leaves[1..], &path));
            assert!(!verify_content_block(&layer.id, size, (index + 1) % 3, &leaves, &path), "block {index} as {}", (index + 1) % 3);
        }
        // The same tree under another size is another content.
        let leaves: Vec<ContentHash> = data.chunks(leaf).take(16).map(content_leaf_hash).collect();
        assert!(!verify_content_block(&layer.id, size + 1, 0, &leaves, &layer.block_proof(0).unwrap()));
    }
}
