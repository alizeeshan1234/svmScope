//! The crate's error type.
//!
//! One rule shapes this enum: a **reverting replay is not an error** — it's a
//! successful observation, reported inside [`crate::replay::ReplayResult`]. An
//! `Err` here always means svmscope itself could not do what was asked: the RPC
//! failed, the transaction doesn't exist, a mutation targeted a missing account,
//! a named field couldn't be resolved. That distinction is what lets a scenario
//! that *expects* a revert never be satisfied by an infrastructure failure.

/// Convenience alias used across the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong short of the transaction itself reverting.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An RPC request failed (network, rate limit, node error).
    #[error("rpc request failed: {0}")]
    Rpc(#[source] Box<solana_client::client_error::ClientError>),

    /// The RPC answered, but not with the shape we expect.
    #[error("unexpected rpc response: {0}")]
    MalformedRpcResponse(String),

    #[error("transaction not found: {0}")]
    TransactionNotFound(String),

    #[error("no transactions found for address {0}")]
    NoSignatures(String),

    #[error("not a valid address: {0}")]
    InvalidAddress(String),

    #[error("account not found: {0}")]
    AccountNotFound(String),

    /// The program publishes no on-chain Anchor IDL (and none was supplied).
    #[error("{0} publishes no on-chain Anchor IDL — supply the IDL JSON to decode against it")]
    NoIdl(String),

    /// A mutation targeted an account the replay never loaded — usually a typo'd
    /// address. Reported as a hard error so an `expect: revert` scenario can
    /// never silently pass because of it.
    #[error("mutation targets an account not loaded in the replay: {0}")]
    MutationTargetMissing(String),

    #[error(
        "patch out of range for {address}: offset {offset} + {len} bytes > account size {size}"
    )]
    PatchOutOfRange {
        address: String,
        offset: usize,
        len: usize,
        size: usize,
    },

    /// A named-field assert referenced a field the account's layout doesn't have.
    #[error("no field \"{field}\" on this {type_name} (fields: {})", available.join(", "))]
    UnknownField {
        field: String,
        type_name: String,
        available: Vec<String>,
    },

    /// A short field name matched several fields — the candidates disambiguate.
    #[error("\"{field}\" is ambiguous here — use one of: {}", candidates.join(", "))]
    AmbiguousField { field: String, candidates: Vec<String> },

    /// Only integer/bool fields can be compared numerically.
    #[error("{field} is a {ty} — only integer/bool fields can be asserted")]
    NonNumericField { field: String, ty: String },

    /// A field or offset read fell outside the account's data.
    #[error("{what} out of range (account data is {len} bytes)")]
    OutOfRange { what: String, len: usize },

    /// The account's bytes couldn't be decoded into named fields.
    #[error(
        "can't decode {address} into named fields (no known layout, and no IDL for its program {owner})"
    )]
    UndecodableAccount { address: String, owner: String },

    /// A transaction's wire bytes couldn't be decoded (bad base64 or bincode).
    #[error("could not decode transaction: {0}")]
    TxDecode(String),

    /// A fixture couldn't be serialized, parsed, or reloaded.
    #[error("fixture error: {0}")]
    Fixture(String),

    /// A suite/scenario/mutation specification was invalid (bad op, kind, hex…).
    #[error("{0}")]
    InvalidSpec(String),
}

impl Error {
    /// Wrap a solana client error (boxed — `ClientError` is large).
    pub(crate) fn rpc(e: solana_client::client_error::ClientError) -> Error {
        Error::Rpc(Box::new(e))
    }
}
