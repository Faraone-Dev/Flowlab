// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

/// Cross-language state verification.
/// Compares hashes produced by Rust core vs C++ hotpath.
pub struct CrossLanguageVerifier {
    rust_hash: u64,
    cpp_hash: u64,
}

impl CrossLanguageVerifier {
    pub fn new() -> Self {
        Self {
            rust_hash: 0,
            cpp_hash: 0,
        }
    }

    pub fn set_rust_hash(&mut self, hash: u64) {
        self.rust_hash = hash;
    }

    pub fn set_cpp_hash(&mut self, hash: u64) {
        self.cpp_hash = hash;
    }

    /// Returns true if hashes match. Hard failure if they don't.
    pub fn verify(&self) -> Result<(), VerificationError> {
        if self.rust_hash == self.cpp_hash {
            Ok(())
        } else {
            Err(VerificationError::HashMismatch {
                rust_hash: self.rust_hash,
                cpp_hash: self.cpp_hash,
            })
        }
    }
}

impl Default for CrossLanguageVerifier {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VerificationError {
    #[error("state divergence: rust={rust_hash:#018x} cpp={cpp_hash:#018x}")]
    HashMismatch { rust_hash: u64, cpp_hash: u64 },
}
