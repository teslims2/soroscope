use sha2::{Digest, Sha256};
use rayon::prelude::*;

/// Represents a Merkle Tree for storing cryptographic state commitments.
pub struct MerkleTree {
    /// The root hash of the tree.
    pub root: [u8; 32],
    /// The level of the tree (max 32 levels).
    pub levels: usize,
    /// The current leaf nodes/data inputs.
    data_leaves: Vec<Vec<u8>>,
}

impl MerkleTree {
    /// Creates a new, empty Merkle Tree.
    pub fn new(levels: usize) -> Self {
        MerkleTree {
            root: [0u8; 32],
            levels,
            data_leaves: Vec::new(),
        }
    }

    /// Builds the Merkle Tree from a provided set of data blocks.
    /// This implementation should handle an efficient, incremental update math.
    pub fn build(&mut self, leaves: Vec<Vec<u8>>) -> Result<(), &'static str> {
        if leaves.is_empty() {
            return Err("Cannot build tree from empty leaves.");
        }
        
        self.data_leaves = leaves.clone();
        self.root.copy_from_slice(&Self::calculate_root_hash(leaves));
        Ok(())
    }

    /// A helper function to perform the recursive root calculation.
    fn calculate_root_hash(mut levels: Vec<Vec<u8>>) -> [u8; 32] {
        while levels.len() > 1 {
            let next_level: Vec<Vec<u8>> = levels
                .par_chunks(2)
                .map(|pair| {
                    let mut hasher = Sha256::new();
                    hasher.update(&pair[0]);
                    if pair.len() == 2 {
                        hasher.update(&pair[1]);
                    } else {
                        hasher.update(&pair[0]); // Hash with itself if odd
                    }
                    hasher.finalize().to_vec()
                })
                .collect();
            levels = next_level;
        }
        
        // Return the final hash (the root)
        let mut root_bytes = [0u8; 32];
        if let Some(root) = levels.first() {
            root_bytes.copy_from_slice(root);
        }
        root_bytes
    }

    /// Gets the root hash as a hex string for easy use in transactions/commitments.
    pub fn get_root_hex(&self) -> String {
        hex::encode(self.root.to_vec())
    }
}

// Minimal implementation required for compilation/testing purposes.
// Note: Full usage requires adding 'hex' and 'sha2' to Cargo.toml
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merkle_tree_basic_commit() {
        let mut tree = MerkleTree::new(32);
        
        // Example data leaves
        let data = vec![b"data1".to_vec(), b"data2".to_vec(), b"data3".to_vec()];
        
        let result = tree.build(data);
        assert!(result.is_ok());
    }
}