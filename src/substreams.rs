// Mirrors of published protobuf messages: the proto is the documentation.
#![allow(missing_docs)]

//! A native client for the Substreams `Stream.Blocks` call.
//!
//! Only what the account-changes stream needs: the request, the response
//! envelope, the module graph carried by a package so it can be sent back
//! with our parameters, and the `FilteredAccounts` output. The messages are
//! written by hand from the published protos
//! (`sf/substreams/rpc/v2/service.proto`, `sf/substreams/v1/modules.proto`,
//! `sf/substreams/v1/package.proto`, `sf/solana/type/v1/account.proto`),
//! so no protoc runs at build time. Fields the client never reads are left
//! out where the message is only decoded (prost skips unknown fields) and
//! kept where the message is sent back (the module graph must round-trip
//! whole).
//!
//! Bytes travel as bytes: a 1.7 MB order book costs 1.7 MB, not the minutes
//! of quadratic base58 the command-line client spends rendering it.

use {
    crate::error::{Error, Result},
    prost::Message,
    std::time::Duration,
};

/// `sf.substreams.v1.Modules`: the module graph and wasm binaries a package
/// carries, sent back whole with every request.
#[derive(Clone, PartialEq, Message)]
pub struct Modules {
    #[prost(message, repeated, tag = "1")]
    pub modules: Vec<Module>,
    #[prost(message, repeated, tag = "2")]
    pub binaries: Vec<Binary>,
}

/// `sf.substreams.v1.Binary`.
#[derive(Clone, PartialEq, Message)]
pub struct Binary {
    #[prost(string, tag = "1")]
    pub r#type: String,
    #[prost(bytes = "vec", tag = "2")]
    pub content: Vec<u8>,
}

/// `sf.substreams.v1.Module`.
#[derive(Clone, PartialEq, Message)]
pub struct Module {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(oneof = "module::Kind", tags = "2, 3, 10")]
    pub kind: Option<module::Kind>,
    #[prost(uint32, tag = "4")]
    pub binary_index: u32,
    #[prost(string, tag = "5")]
    pub binary_entrypoint: String,
    #[prost(message, repeated, tag = "6")]
    pub inputs: Vec<module::Input>,
    #[prost(message, optional, tag = "7")]
    pub output: Option<module::Output>,
    #[prost(uint64, tag = "8")]
    pub initial_block: u64,
    #[prost(message, optional, tag = "9")]
    pub block_filter: Option<module::BlockFilter>,
}

pub mod module {
    use prost::{Message, Oneof};

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Kind {
        #[prost(message, tag = "2")]
        KindMap(KindMap),
        #[prost(message, tag = "3")]
        KindStore(KindStore),
        #[prost(message, tag = "10")]
        KindBlockIndex(KindBlockIndex),
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct KindMap {
        #[prost(string, tag = "1")]
        pub output_type: String,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct KindStore {
        #[prost(int32, tag = "1")]
        pub update_policy: i32,
        #[prost(string, tag = "2")]
        pub value_type: String,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct KindBlockIndex {
        #[prost(string, tag = "1")]
        pub output_type: String,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct BlockFilter {
        #[prost(string, tag = "1")]
        pub module: String,
        #[prost(oneof = "block_filter::Query", tags = "2, 3")]
        pub query: Option<block_filter::Query>,
    }

    pub mod block_filter {
        use prost::{Message, Oneof};

        #[derive(Clone, PartialEq, Oneof)]
        pub enum Query {
            #[prost(string, tag = "2")]
            QueryString(String),
            #[prost(message, tag = "3")]
            QueryFromParams(QueryFromParams),
        }

        #[derive(Clone, PartialEq, Message)]
        pub struct QueryFromParams {}
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct Input {
        #[prost(oneof = "input::Input", tags = "1, 2, 3, 4, 5")]
        pub input: Option<input::Input>,
    }

    pub mod input {
        use prost::{Message, Oneof};

        #[derive(Clone, PartialEq, Oneof)]
        pub enum Input {
            #[prost(message, tag = "1")]
            Source(Source),
            #[prost(message, tag = "2")]
            Map(Map),
            #[prost(message, tag = "3")]
            Store(Store),
            #[prost(message, tag = "4")]
            Params(Params),
            #[prost(message, tag = "5")]
            FoundationalStore(FoundationalStore),
        }

        #[derive(Clone, PartialEq, Message)]
        pub struct Source {
            #[prost(string, tag = "1")]
            pub r#type: String,
        }

        #[derive(Clone, PartialEq, Message)]
        pub struct Map {
            #[prost(string, tag = "1")]
            pub module_name: String,
        }

        #[derive(Clone, PartialEq, Message)]
        pub struct Store {
            #[prost(string, tag = "1")]
            pub module_name: String,
            #[prost(int32, tag = "2")]
            pub mode: i32,
        }

        #[derive(Clone, PartialEq, Message)]
        pub struct Params {
            #[prost(string, tag = "1")]
            pub value: String,
        }

        #[derive(Clone, PartialEq, Message)]
        pub struct FoundationalStore {
            #[prost(string, tag = "1")]
            pub identifier: String,
        }
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct Output {
        #[prost(string, tag = "1")]
        pub r#type: String,
    }
}

/// `sf.substreams.v1.Package`, the parts we read: the module graph. The
/// proto descriptors and metadata are skipped.
#[derive(Clone, PartialEq, Message)]
pub struct Package {
    #[prost(message, optional, tag = "6")]
    pub modules: Option<Modules>,
}

/// `sf.substreams.rpc.v2.Request`.
#[derive(Clone, PartialEq, Message)]
pub struct Request {
    #[prost(int64, tag = "1")]
    pub start_block_num: i64,
    #[prost(string, tag = "2")]
    pub start_cursor: String,
    #[prost(uint64, tag = "3")]
    pub stop_block_num: u64,
    #[prost(bool, tag = "4")]
    pub final_blocks_only: bool,
    #[prost(bool, tag = "5")]
    pub production_mode: bool,
    #[prost(string, tag = "6")]
    pub output_module: String,
    #[prost(message, optional, tag = "7")]
    pub modules: Option<Modules>,
    #[prost(bool, tag = "11")]
    pub noop_mode: bool,
    #[prost(uint64, tag = "12")]
    pub limit_processed_blocks: u64,
    #[prost(uint64, tag = "14")]
    pub progress_messages_interval_ms: u64,
}

/// `sf.substreams.rpc.v2.Response`, the variants we act on. Progress and
/// session messages are decoded as unknown and dropped.
#[derive(Clone, PartialEq, Message)]
pub struct Response {
    #[prost(oneof = "response::Payload", tags = "3, 4, 5")]
    pub message: Option<response::Payload>,
}

pub mod response {
    use prost::{Message, Oneof};

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Payload {
        #[prost(message, tag = "3")]
        BlockScopedData(BlockScopedData),
        #[prost(message, tag = "4")]
        BlockUndoSignal(BlockUndoSignal),
        #[prost(message, tag = "5")]
        FatalError(Error),
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct BlockScopedData {
        #[prost(message, optional, tag = "1")]
        pub output: Option<MapModuleOutput>,
        #[prost(message, optional, tag = "2")]
        pub clock: Option<Clock>,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct MapModuleOutput {
        #[prost(string, tag = "1")]
        pub name: String,
        #[prost(message, optional, tag = "2")]
        pub map_output: Option<Any>,
    }

    /// `google.protobuf.Any`.
    #[derive(Clone, PartialEq, Message)]
    pub struct Any {
        #[prost(string, tag = "1")]
        pub type_url: String,
        #[prost(bytes = "vec", tag = "2")]
        pub value: Vec<u8>,
    }

    /// `sf.substreams.v1.Clock`, without the timestamp.
    #[derive(Clone, PartialEq, Message)]
    pub struct Clock {
        #[prost(string, tag = "1")]
        pub id: String,
        #[prost(uint64, tag = "2")]
        pub number: u64,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct BlockUndoSignal {}

    #[derive(Clone, PartialEq, Message)]
    pub struct Error {
        #[prost(string, tag = "1")]
        pub module: String,
        #[prost(string, tag = "2")]
        pub reason: String,
        #[prost(string, repeated, tag = "3")]
        pub logs: Vec<String>,
    }
}

/// `sf.substreams.solana.type.v1.FilteredAccounts`: the module's output.
#[derive(Clone, PartialEq, Message)]
pub struct FilteredAccounts {
    #[prost(message, repeated, tag = "1")]
    pub accounts: Vec<Account>,
}

/// `sf.solana.type.v1.Account`: one changed account's bytes at the end of a
/// block. No lamports.
#[derive(Clone, PartialEq, Message)]
pub struct Account {
    #[prost(bytes = "vec", tag = "1")]
    pub address: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub owner: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub data: Vec<u8>,
    #[prost(bool, tag = "7")]
    pub deleted: bool,
}

/// The header the API key travels in.
const API_KEY_HEADER: &str = "x-api-key";
/// The gRPC method.
const BLOCKS_PATH: &str = "/sf.substreams.rpc.v2.Stream/Blocks";
/// A block's output can hold several large accounts; leave room.
const MAX_MESSAGE_BYTES: usize = 256 * 1024 * 1024;

/// One block's changed accounts, as the module filtered them.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountsAt {
    pub slot: u64,
    pub accounts: Vec<Account>,
}

/// What a call to the stream needs.
#[derive(Debug, Clone)]
pub struct Call<'a> {
    /// `https://host:port`.
    pub endpoint: &'a str,
    pub api_key: &'a str,
    pub modules: &'a Modules,
    /// The map module to stream.
    pub output_module: &'a str,
    /// The params value for `output_module` (replaces the package default).
    pub params: &'a str,
    /// First block, inclusive.
    pub start: u64,
    /// Last block, exclusive.
    pub stop: u64,
    /// Give up after this long.
    pub deadline: Duration,
}

/// Parse a package (`.spkg` bytes) down to its module graph.
pub fn package_modules(spkg: &[u8]) -> Result<Modules> {
    let package = Package::decode(spkg)
        .map_err(|e| Error::Fixture(format!("substreams package: decode: {e}")))?;
    package
        .modules
        .ok_or_else(|| Error::Fixture("substreams package: no modules".into()))
}

/// The module graph with `module`'s params input set to `value`.
pub fn with_params(modules: &Modules, module: &str, value: &str) -> Modules {
    let mut out = modules.clone();
    for m in out.modules.iter_mut().filter(|m| m.name == module) {
        for input in &mut m.inputs {
            if let Some(module::input::Input::Params(p)) = input.input.as_mut() {
                p.value = value.to_string();
            }
        }
    }
    out
}

/// Whether a failure is the provider's per-account concurrency limit.
pub fn is_stream_limit(message: &str) -> bool {
    message.contains("Concurrent stream limit exceeded")
        || message.contains("concurrent stream limit")
}

/// Whether a failure is the provider's monthly egress quota, which no
/// retry will clear: the key is done until the quota resets.
pub fn is_quota_exhausted(message: &str) -> bool {
    message.contains("quota exceeded") || message.contains("Quota exceeded")
}

/// Stream `[start, stop)` and return every block's filtered accounts, in
/// block order. Blocks with no matching account are not sent by the server
/// in production mode. Runs its own single-threaded runtime: the rest of
/// the library is blocking.
pub fn stream_accounts(call: &Call<'_>) -> Result<Vec<AccountsAt>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Fixture(format!("substreams: runtime: {e}")))?;
    // The timer needs the runtime's reactor, so it is built inside.
    let deadline = call.deadline;
    match runtime
        .block_on(async { tokio::time::timeout(deadline, stream_accounts_async(call)).await })
    {
        Ok(r) => r,
        Err(_) => Err(Error::Fixture(format!(
            "substreams: no answer within {} s",
            call.deadline.as_secs()
        ))),
    }
}

async fn stream_accounts_async(call: &Call<'_>) -> Result<Vec<AccountsAt>> {
    use tonic::transport::{ClientTlsConfig, Endpoint};
    let fail =
        |what: &str, e: &dyn std::fmt::Display| Error::Fixture(format!("substreams: {what}: {e}"));
    let endpoint = Endpoint::from_shared(call.endpoint.to_string())
        .map_err(|e| fail("endpoint", &e))?
        .tls_config(ClientTlsConfig::new().with_webpki_roots())
        .map_err(|e| fail("tls", &e))?
        .connect_timeout(Duration::from_secs(20))
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .http2_keep_alive_interval(Duration::from_secs(30));
    let channel = endpoint.connect().await.map_err(|e| fail("connect", &e))?;
    // The server refuses a client that offers no compression it knows.
    let mut grpc = tonic::client::Grpc::new(channel)
        .accept_compressed(tonic::codec::CompressionEncoding::Gzip)
        .send_compressed(tonic::codec::CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);
    grpc.ready().await.map_err(|e| fail("ready", &e))?;

    let modules = with_params(call.modules, call.output_module, call.params);
    let mut request = tonic::Request::new(Request {
        start_block_num: call.start as i64,
        start_cursor: String::new(),
        stop_block_num: call.stop,
        final_blocks_only: true,
        production_mode: true,
        output_module: call.output_module.to_string(),
        modules: Some(modules),
        noop_mode: false,
        limit_processed_blocks: 0,
        // A long range over a quiet account sends no data for minutes;
        // progress messages keep the connection from being cut as idle.
        progress_messages_interval_ms: 5_000,
    });
    let key = call
        .api_key
        .parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>()
        .map_err(|e| fail("api key", &e))?;
    request.metadata_mut().insert(API_KEY_HEADER, key);
    let path = BLOCKS_PATH
        .parse::<http::uri::PathAndQuery>()
        .map_err(|e| fail("path", &e))?;
    let codec = tonic_prost::ProstCodec::<Request, Response>::default();
    let mut stream = grpc
        .server_streaming(request, path, codec)
        .await
        .map_err(|s| Error::Fixture(format!("substreams: {}: {}", s.code(), s.message())))?
        .into_inner();

    let mut out = Vec::new();
    loop {
        let next = stream
            .message()
            .await
            .map_err(|s| Error::Fixture(format!("substreams: {}: {}", s.code(), s.message())))?;
        let Some(response) = next else {
            break;
        };
        match response.message {
            Some(response::Payload::BlockScopedData(block)) => {
                let slot = block.clock.map(|c| c.number).unwrap_or(0);
                let Some(any) = block.output.and_then(|o| o.map_output) else {
                    continue;
                };
                let accounts = FilteredAccounts::decode(any.value.as_slice())
                    .map_err(|e| fail("decode output", &e))?
                    .accounts;
                if !accounts.is_empty() {
                    out.push(AccountsAt { slot, accounts });
                }
            }
            Some(response::Payload::FatalError(e)) => {
                return Err(Error::Fixture(format!(
                    "substreams: module {}: {}",
                    e.module, e.reason
                )));
            }
            // Final blocks only: no undo signals; anything else is progress.
            Some(response::Payload::BlockUndoSignal(_)) | None => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPKG: &[u8] = include_bytes!("../assets/solana-accounts-foundational-v0.1.1.spkg");

    #[test]
    fn the_bundled_package_round_trips_with_our_params() {
        let modules = package_modules(SPKG).unwrap();
        let names: Vec<&str> = modules.modules.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["index_accounts", "filtered_accounts"]);
        assert_eq!(modules.binaries.len(), 1);
        assert!(modules.binaries[0].content.len() > 10_000);
        let filtered = &modules.modules[1];
        assert!(matches!(filtered.kind, Some(module::Kind::KindMap(_))));
        assert!(matches!(
            filtered
                .block_filter
                .as_ref()
                .and_then(|f| f.query.as_ref()),
            Some(module::block_filter::Query::QueryFromParams(_))
        ));
        let set = with_params(&modules, "filtered_accounts", "account:X || account:Y");
        let params: Vec<&str> = set.modules[1]
            .inputs
            .iter()
            .filter_map(|i| match &i.input {
                Some(module::input::Input::Params(p)) => Some(p.value.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(params, ["account:X || account:Y"]);
        // Every byte of the graph survives a re-encode: the server gets what
        // the package author built.
        let again = Modules::decode(modules.encode_to_vec().as_slice()).unwrap();
        assert_eq!(again, modules);
    }

    #[test]
    fn a_block_output_decodes_to_accounts() {
        let accounts = FilteredAccounts {
            accounts: vec![Account {
                address: vec![1; 32],
                owner: vec![2; 32],
                data: vec![3; 5],
                deleted: false,
            }],
        };
        let response = Response {
            message: Some(response::Payload::BlockScopedData(
                response::BlockScopedData {
                    output: Some(response::MapModuleOutput {
                        name: "filtered_accounts".into(),
                        map_output: Some(response::Any {
                            type_url:
                                "type.googleapis.com/sf.substreams.solana.type.v1.FilteredAccounts"
                                    .into(),
                            value: accounts.encode_to_vec(),
                        }),
                    }),
                    clock: Some(response::Clock {
                        id: "h".into(),
                        number: 42,
                    }),
                },
            )),
        };
        let decoded = Response::decode(response.encode_to_vec().as_slice()).unwrap();
        let Some(response::Payload::BlockScopedData(b)) = decoded.message else {
            panic!("wrong variant");
        };
        assert_eq!(b.clock.unwrap().number, 42);
        let got = FilteredAccounts::decode(b.output.unwrap().map_output.unwrap().value.as_slice())
            .unwrap();
        assert_eq!(got, accounts);
    }
}
