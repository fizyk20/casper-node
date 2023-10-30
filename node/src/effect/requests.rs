//! Request effects.
//!
//! Requests typically ask other components to perform a service and report back the result. See the
//! top-level module documentation for details.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::{self, Debug, Display, Formatter},
    mem,
    sync::Arc,
};

use datasize::DataSize;
use num_rational::Ratio;
use serde::Serialize;
use smallvec::SmallVec;
use static_assertions::const_assert;

use casper_execution_engine::engine_state::{
    self,
    balance::{BalanceRequest, BalanceResult},
    era_validators::GetEraValidatorsError,
    get_bids::{GetBidsRequest, GetBidsResult},
    query::{QueryRequest, QueryResult},
};
use casper_storage::global_state::trie::TrieRaw;
use casper_types::{
    addressable_entity::AddressableEntity,
    bytesrepr::Bytes,
    execution::{ExecutionResult, ExecutionResultV2},
    system::auction::EraValidators,
    Block, BlockHash, BlockHeader, BlockSignatures, BlockV2, ChainspecRawBytes, DeployHash,
    DeployHeader, Digest, DisplayIter, EraId, FinalitySignature, FinalitySignatureId, Key,
    ProtocolVersion, PublicKey, TimeDiff, Timestamp, Transaction, TransactionHash, TransactionId,
    Transfer, URef, U512,
};

use super::{AutoClosingResponder, GossipTarget, Responder};
use crate::{
    components::{
        block_synchronizer::{
            BlockSynchronizerStatus, GlobalStateSynchronizerError, GlobalStateSynchronizerResponse,
            TrieAccumulatorError, TrieAccumulatorResponse,
        },
        consensus::{ClContext, ProposedBlock, ValidatorChange},
        contract_runtime::EraValidatorsRequest,
        diagnostics_port::StopAtSpec,
        fetcher::{FetchItem, FetchResult},
        gossiper::GossipItem,
        network::NetworkInsights,
        transaction_acceptor,
        upgrade_watcher::NextUpgrade,
    },
    contract_runtime::{
        ContractRuntimeError, RoundSeigniorageRateRequest, SpeculativeExecutionState,
        TotalSupplyRequest,
    },
    reactor::main_reactor::ReactorState,
    rpcs::docs::OpenRpcSchema,
    types::{
        appendable_block::AppendableBlock, ApprovalsHashes, AvailableBlockRange,
        BlockExecutionResultsOrChunk, BlockExecutionResultsOrChunkId, BlockWithMetadata,
        ExecutableBlock, ExecutionInfo, FinalizedApprovals, LegacyDeploy, MetaBlockState, NodeId,
        SignedBlock, StatusFeed, TransactionWithFinalizedApprovals, TrieOrChunk, TrieOrChunkId,
    },
    utils::Source,
};

const _STORAGE_REQUEST_SIZE: usize = mem::size_of::<StorageRequest>();
const_assert!(_STORAGE_REQUEST_SIZE < 89);

/// A metrics request.
#[derive(Debug)]
pub(crate) enum MetricsRequest {
    /// Render current node metrics as prometheus-formatted string.
    RenderNodeMetricsText {
        /// Responder returning the rendered metrics or `None`, if an internal error occurred.
        responder: Responder<Option<String>>,
    },
}

impl Display for MetricsRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            MetricsRequest::RenderNodeMetricsText { .. } => write!(formatter, "get metrics text"),
        }
    }
}

const _NETWORK_EVENT_SIZE: usize = mem::size_of::<NetworkRequest<String>>();
const_assert!(_NETWORK_EVENT_SIZE < 105);

/// A networking request.
#[derive(Debug, Serialize)]
#[must_use]
pub(crate) enum NetworkRequest<P> {
    /// Send a message on the network to a specific peer.
    SendMessage {
        /// Message destination.
        dest: Box<NodeId>,
        /// Message payload.
        payload: Box<P>,
        /// If `true`, the responder will be called early after the message has been queued, not
        /// waiting until it has passed to the kernel.
        respond_after_queueing: bool,
        /// Responder to be called when the message has been *buffered for sending*.
        #[serde(skip_serializing)]
        auto_closing_responder: AutoClosingResponder<()>,
    },
    /// Send a message on the network to validator peers in the given era.
    ValidatorBroadcast {
        /// Message payload.
        payload: Box<P>,
        /// Era whose validators are recipients.
        era_id: EraId,
        /// Responder to be called when all messages are queued.
        #[serde(skip_serializing)]
        auto_closing_responder: AutoClosingResponder<()>,
    },
    /// Gossip a message to a random subset of peers.
    Gossip {
        /// Payload to gossip.
        payload: Box<P>,
        /// Type of peers that should receive the gossip message.
        gossip_target: GossipTarget,
        /// Number of peers to gossip to. This is an upper bound, otherwise best-effort.
        count: usize,
        /// Node IDs of nodes to exclude from gossiping to.
        #[serde(skip_serializing)]
        exclude: HashSet<NodeId>,
        /// Responder to be called when all messages are queued.
        #[serde(skip_serializing)]
        auto_closing_responder: AutoClosingResponder<HashSet<NodeId>>,
    },
}

impl<P> NetworkRequest<P> {
    /// Transform a network request by mapping the contained payload.
    ///
    /// This is a replacement for a `From` conversion that is not possible without specialization.
    pub(crate) fn map_payload<F, P2>(self, wrap_payload: F) -> NetworkRequest<P2>
    where
        F: FnOnce(P) -> P2,
    {
        match self {
            NetworkRequest::SendMessage {
                dest,
                payload,
                respond_after_queueing,
                auto_closing_responder,
            } => NetworkRequest::SendMessage {
                dest,
                payload: Box::new(wrap_payload(*payload)),
                respond_after_queueing,
                auto_closing_responder,
            },
            NetworkRequest::ValidatorBroadcast {
                payload,
                era_id,
                auto_closing_responder,
            } => NetworkRequest::ValidatorBroadcast {
                payload: Box::new(wrap_payload(*payload)),
                era_id,
                auto_closing_responder,
            },
            NetworkRequest::Gossip {
                payload,
                gossip_target,
                count,
                exclude,
                auto_closing_responder,
            } => NetworkRequest::Gossip {
                payload: Box::new(wrap_payload(*payload)),
                gossip_target,
                count,
                exclude,
                auto_closing_responder,
            },
        }
    }
}

impl<P> Display for NetworkRequest<P>
where
    P: Display,
{
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            NetworkRequest::SendMessage { dest, payload, .. } => {
                write!(formatter, "send to {}: {}", dest, payload)
            }
            NetworkRequest::ValidatorBroadcast { payload, .. } => {
                write!(formatter, "broadcast: {}", payload)
            }
            NetworkRequest::Gossip { payload, .. } => write!(formatter, "gossip: {}", payload),
        }
    }
}

/// A networking info request.
#[derive(Debug, Serialize)]
pub(crate) enum NetworkInfoRequest {
    /// Get incoming and outgoing peers.
    Peers {
        /// Responder to be called with all connected peers.
        /// Responds with a map from [NodeId]s to a socket address, represented as a string.
        responder: Responder<BTreeMap<NodeId, String>>,
    },
    /// Get up to `count` fully-connected peers in random order.
    FullyConnectedPeers {
        count: usize,
        /// Responder to be called with the peers.
        responder: Responder<Vec<NodeId>>,
    },
    /// Get detailed insights into the nodes networking.
    Insight {
        responder: Responder<NetworkInsights>,
    },
}

impl Display for NetworkInfoRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            NetworkInfoRequest::Peers { responder: _ } => {
                formatter.write_str("get peers-to-socket-address map")
            }
            NetworkInfoRequest::FullyConnectedPeers {
                count,
                responder: _,
            } => {
                write!(formatter, "get up to {} fully connected peers", count)
            }
            NetworkInfoRequest::Insight { responder: _ } => {
                formatter.write_str("get networking insights")
            }
        }
    }
}

/// A gossip request.
///
/// This request usually initiates gossiping process of the specified item. Note that the gossiper
/// will fetch the item itself, so only the ID is needed.
///
/// The responder will be called as soon as the gossiper has initiated the process.
// Note: This request should eventually entirely replace `ItemReceived`.
#[derive(Debug, Serialize)]
#[must_use]
pub(crate) struct BeginGossipRequest<T>
where
    T: GossipItem,
{
    pub(crate) item_id: T::Id,
    pub(crate) source: Source,
    pub(crate) target: GossipTarget,
    pub(crate) responder: Responder<()>,
}

impl<T> Display for BeginGossipRequest<T>
where
    T: GossipItem,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "begin gossip of {} from {}", self.item_id, self.source)
    }
}

#[derive(Debug, Serialize)]
/// A storage request.
pub(crate) enum StorageRequest {
    /// Store given block.
    PutBlock {
        /// Block to be stored.
        block: Arc<Block>,
        /// Responder to call with the result.  Returns true if the block was stored on this
        /// attempt or false if it was previously stored.
        responder: Responder<bool>,
    },
    /// Store the approvals hashes.
    PutApprovalsHashes {
        /// Approvals hashes to store.
        approvals_hashes: Box<ApprovalsHashes>,
        responder: Responder<bool>,
    },
    /// Store the block and approvals hashes.
    PutExecutedBlock {
        /// Block to be stored.
        block: Arc<BlockV2>,
        /// Approvals hashes to store.
        approvals_hashes: Box<ApprovalsHashes>,
        execution_results: HashMap<DeployHash, ExecutionResult>,
        responder: Responder<bool>,
    },
    /// Retrieve block with given hash.
    GetBlock {
        /// Hash of block to be retrieved.
        block_hash: BlockHash,
        /// Responder to call with the result.  Returns `None` if the block doesn't exist in local
        /// storage.
        responder: Responder<Option<Block>>,
    },
    IsBlockStored {
        block_hash: BlockHash,
        responder: Responder<bool>,
    },
    /// Retrieve the approvals hashes.
    GetApprovalsHashes {
        /// Hash of the block for which to retrieve approvals hashes.
        block_hash: BlockHash,
        /// Responder to call with the result.  Returns `None` if the approvals hashes don't exist
        /// in local storage.
        responder: Responder<Option<ApprovalsHashes>>,
    },
    /// Retrieve highest complete block.
    GetHighestCompleteBlock {
        /// Responder.
        responder: Responder<Option<Block>>,
    },
    /// Retrieve highest complete block header.
    GetHighestCompleteBlockHeader {
        /// Responder.
        responder: Responder<Option<BlockHeader>>,
    },
    /// Retrieve the header of the block containing the deploy.
    GetBlockHeaderForDeploy {
        /// Hash of the deploy.
        deploy_hash: DeployHash,
        /// Responder.
        responder: Responder<Option<BlockHeader>>,
    },
    /// Retrieve block header with given hash.
    GetBlockHeader {
        /// Hash of block to get header of.
        block_hash: BlockHash,
        /// If true, only return `Some` if the block is in the available block range, i.e. the
        /// highest contiguous range of complete blocks.
        only_from_available_block_range: bool,
        /// Responder to call with the result.  Returns `None` if the block header doesn't exist in
        /// local storage.
        responder: Responder<Option<BlockHeader>>,
    },
    GetBlockHeaderByHeight {
        /// Height of block to get header of.
        block_height: u64,
        /// If true, only return `Some` if the block is in the available block range, i.e. the
        /// highest contiguous range of complete blocks.
        only_from_available_block_range: bool,
        /// Responder to call with the result.  Returns `None` if the block header doesn't exist in
        /// local storage.
        responder: Responder<Option<BlockHeader>>,
    },
    GetSwitchBlockHeaderByEra {
        /// Era ID for which to get the block header.
        era_id: EraId,
        /// Responder to call with the result.
        responder: Responder<Option<BlockHeader>>,
    },
    /// Retrieve all transfers in a block with given hash.
    GetBlockTransfers {
        /// Hash of block to get transfers of.
        block_hash: BlockHash,
        /// Responder to call with the result.  Returns `None` if the transfers do not exist in
        /// local storage under the block_hash provided.
        responder: Responder<Option<Vec<Transfer>>>,
    },
    PutTransaction {
        transaction: Arc<Transaction>,
        /// Returns `true` if the transaction was stored on this attempt or false if it was
        /// previously stored.
        responder: Responder<bool>,
    },
    /// Retrieve transaction with given hashes.
    GetTransactions {
        transaction_hashes: Vec<TransactionHash>,
        responder: Responder<SmallVec<[Option<TransactionWithFinalizedApprovals>; 1]>>,
    },
    /// Retrieve legacy deploy with given hash.
    GetLegacyDeploy {
        deploy_hash: DeployHash,
        responder: Responder<Option<LegacyDeploy>>,
    },
    GetTransaction {
        transaction_id: TransactionId,
        responder: Responder<Option<Transaction>>,
    },
    IsTransactionStored {
        transaction_id: TransactionId,
        responder: Responder<bool>,
    },
    /// Store execution results for a set of deploys of a single block.
    ///
    /// Will return a fatal error if there are already execution results known for a specific
    /// deploy/block combination and a different result is inserted.
    ///
    /// Inserting the same block/deploy combination multiple times with the same execution results
    /// is not an error and will silently be ignored.
    PutExecutionResults {
        /// Hash of block.
        block_hash: Box<BlockHash>,
        block_height: u64,
        /// Mapping of deploys to execution results of the block.
        execution_results: HashMap<DeployHash, ExecutionResult>,
        /// Responder to call when done storing.
        responder: Responder<()>,
    },
    GetExecutionResults {
        block_hash: BlockHash,
        responder: Responder<Option<Vec<(DeployHash, DeployHeader, ExecutionResult)>>>,
    },
    GetBlockExecutionResultsOrChunk {
        /// Request ID.
        id: BlockExecutionResultsOrChunkId,
        /// Responder to call with the execution results.
        /// None is returned when we don't have the block in the storage.
        responder: Responder<Option<BlockExecutionResultsOrChunk>>,
    },
    GetTransactionAndExecutionInfo {
        transaction_hash: TransactionHash,
        responder: Responder<Option<(TransactionWithFinalizedApprovals, Option<ExecutionInfo>)>>,
    },
    /// Retrieve block and its signatures by its hash.
    GetSignedBlockByHash {
        /// The hash of the block.
        block_hash: BlockHash,
        /// If true, only return `Some` if the block is in the available block range, i.e. the
        /// highest contiguous range of complete blocks.
        only_from_available_block_range: bool,
        /// The responder to call with the results.
        responder: Responder<Option<SignedBlock>>,
    },
    /// Retrieve a finality signature by block hash and public key.
    GetFinalitySignature {
        id: Box<FinalitySignatureId>,
        responder: Responder<Option<FinalitySignature>>,
    },
    IsFinalitySignatureStored {
        id: Box<FinalitySignatureId>,
        responder: Responder<bool>,
    },
    /// Retrieve block and its signatures at a given height.
    GetSignedBlockByHeight {
        /// The height of the block.
        block_height: BlockHeight,
        /// If true, only return `Some` if the block is in the available block range, i.e. the
        /// highest contiguous range of complete blocks.
        only_from_available_block_range: bool,
        /// The responder to call with the results.
        responder: Responder<Option<SignedBlock>>,
    },
    /// Retrieve block and its metadata at a given height.
    GetBlockAndMetadataByHeight {
        /// The height of the block.
        block_height: BlockHeight,
        /// Flag indicating whether storage should check the block availability before trying to
        /// retrieve it.
        only_from_available_block_range: bool,
        /// The responder to call with the results.
        responder: Responder<Option<BlockWithMetadata>>,
    },
    /// Get the highest block and its signatures.
    GetHighestSignedBlock {
        /// If true, only consider blocks in the available block range, i.e. the highest contiguous
        /// range of complete blocks.
        only_from_available_block_range: bool,
        /// The responder to call the results with.
        responder: Responder<Option<SignedBlock>>,
    },
    /// Get a single finality signature for a block hash.
    GetBlockSignature {
        /// The hash for the request.
        block_hash: BlockHash,
        /// The public key of the signer.
        public_key: Box<PublicKey>,
        /// Responder to call with the result.
        responder: Responder<Option<FinalitySignature>>,
    },
    /// Store finality signatures.
    PutBlockSignatures {
        /// Signatures that are to be stored.
        signatures: BlockSignatures,
        /// Responder to call with the result, if true then the signatures were successfully
        /// stored.
        responder: Responder<bool>,
    },
    PutFinalitySignature {
        signature: Box<FinalitySignature>,
        responder: Responder<bool>,
    },
    /// Store a block header.
    PutBlockHeader {
        /// Block header that is to be stored.
        block_header: Box<BlockHeader>,
        /// Responder to call with the result, if true then the block header was successfully
        /// stored.
        responder: Responder<bool>,
    },
    /// Retrieve the height range of fully available blocks (not just block headers). Returns
    /// `[u64::MAX, u64::MAX]` when there are no sequences.
    GetAvailableBlockRange {
        /// Responder to call with the result.
        responder: Responder<AvailableBlockRange>,
    },
    /// Store a set of finalized approvals for a specific transaction.
    StoreFinalizedApprovals {
        /// The transaction hash to store the finalized approvals for.
        transaction_hash: TransactionHash,
        /// The set of finalized approvals.
        finalized_approvals: FinalizedApprovals,
        /// Responder, responded to once the approvals are written.  If true, new approvals were
        /// written.
        responder: Responder<bool>,
    },
    /// Retrieve the height of the final block of the previous protocol version, if known.
    GetKeyBlockHeightForActivationPoint { responder: Responder<Option<u64>> },
}

impl Display for StorageRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            StorageRequest::PutBlock { block, .. } => {
                write!(formatter, "put {}", block)
            }
            StorageRequest::PutApprovalsHashes {
                approvals_hashes, ..
            } => {
                write!(formatter, "put {}", approvals_hashes)
            }
            StorageRequest::GetBlock { block_hash, .. } => {
                write!(formatter, "get block {}", block_hash)
            }
            StorageRequest::IsBlockStored { block_hash, .. } => {
                write!(formatter, "is block {} stored", block_hash)
            }
            StorageRequest::GetApprovalsHashes { block_hash, .. } => {
                write!(formatter, "get approvals hashes {}", block_hash)
            }
            StorageRequest::GetHighestCompleteBlock { .. } => {
                write!(formatter, "get highest complete block")
            }
            StorageRequest::GetHighestCompleteBlockHeader { .. } => {
                write!(formatter, "get highest complete block header")
            }
            StorageRequest::GetBlockHeaderForDeploy { deploy_hash, .. } => {
                write!(formatter, "get block header for deploy {}", deploy_hash)
            }
            StorageRequest::GetBlockHeader { block_hash, .. } => {
                write!(formatter, "get {}", block_hash)
            }
            StorageRequest::GetBlockHeaderByHeight { block_height, .. } => {
                write!(formatter, "get header for height {}", block_height)
            }
            StorageRequest::GetSwitchBlockHeaderByEra { era_id, .. } => {
                write!(formatter, "get header for era {}", era_id)
            }
            StorageRequest::GetBlockTransfers { block_hash, .. } => {
                write!(formatter, "get transfers for {}", block_hash)
            }
            StorageRequest::PutTransaction { transaction, .. } => {
                write!(formatter, "put {}", transaction)
            }
            StorageRequest::GetTransactions {
                transaction_hashes, ..
            } => {
                write!(
                    formatter,
                    "get {}",
                    DisplayIter::new(transaction_hashes.iter())
                )
            }
            StorageRequest::GetLegacyDeploy { deploy_hash, .. } => {
                write!(formatter, "get legacy deploy {}", deploy_hash)
            }
            StorageRequest::GetTransaction { transaction_id, .. } => {
                write!(formatter, "get transaction {}", transaction_id)
            }
            StorageRequest::IsTransactionStored { transaction_id, .. } => {
                write!(formatter, "is transaction {} stored", transaction_id)
            }
            StorageRequest::PutExecutionResults { block_hash, .. } => {
                write!(formatter, "put execution results for {}", block_hash)
            }
            StorageRequest::GetExecutionResults { block_hash, .. } => {
                write!(formatter, "get execution results for {}", block_hash)
            }
            StorageRequest::GetBlockExecutionResultsOrChunk { id, .. } => {
                write!(formatter, "get block execution results or chunk for {}", id)
            }

            StorageRequest::GetTransactionAndExecutionInfo {
                transaction_hash, ..
            } => {
                write!(
                    formatter,
                    "get transaction and metadata for {}",
                    transaction_hash
                )
            }
            StorageRequest::GetFinalitySignature { id, .. } => {
                write!(formatter, "get finality signature {}", id)
            }
            StorageRequest::IsFinalitySignatureStored { id, .. } => {
                write!(formatter, "is finality signature {} stored", id)
            }
            StorageRequest::GetSignedBlockByHash { block_hash, .. } => {
                write!(
                    formatter,
                    "get signed block for block with hash: {}",
                    block_hash
                )
            }
            StorageRequest::GetBlockAndMetadataByHeight { block_height, .. } => {
                write!(
                    formatter,
                    "get block and metadata for block at height: {}",
                    block_height
                )
            }
            StorageRequest::GetSignedBlockByHeight { block_height, .. } => {
                write!(
                    formatter,
                    "get signed block for block at height: {}",
                    block_height
                )
            }
            StorageRequest::GetHighestSignedBlock { .. } => {
                write!(formatter, "get highest signed block")
            }
            StorageRequest::GetBlockSignature {
                block_hash,
                public_key,
                ..
            } => {
                write!(
                    formatter,
                    "get finality signature for block hash {} from {}",
                    block_hash, public_key
                )
            }
            StorageRequest::PutBlockSignatures { .. } => {
                write!(formatter, "put finality signatures")
            }
            StorageRequest::PutFinalitySignature { .. } => {
                write!(formatter, "put finality signature")
            }
            StorageRequest::PutBlockHeader { block_header, .. } => {
                write!(formatter, "put block header: {}", block_header)
            }
            StorageRequest::GetAvailableBlockRange { .. } => {
                write!(formatter, "get available block range",)
            }
            StorageRequest::StoreFinalizedApprovals {
                transaction_hash: deploy_hash,
                ..
            } => {
                write!(formatter, "finalized approvals for deploy {}", deploy_hash)
            }
            StorageRequest::PutExecutedBlock { block, .. } => {
                write!(formatter, "put executed block {}", block.hash(),)
            }
            StorageRequest::GetKeyBlockHeightForActivationPoint { .. } => {
                write!(
                    formatter,
                    "get key block height for current activation point"
                )
            }
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct MakeBlockExecutableRequest {
    /// Hash of the block to be made executable.
    pub block_hash: BlockHash,
    /// Responder with the executable block and it's deploys
    pub responder: Responder<Option<ExecutableBlock>>,
}

impl Display for MakeBlockExecutableRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "block made executable: {}", self.block_hash)
    }
}

/// A request to mark a block at a specific height completed.
///
/// A block is considered complete if
///
/// * the block header and the actual block are persisted in storage,
/// * all of its deploys are persisted in storage, and
/// * the global state root the block refers to has no missing dependencies locally.
#[derive(Debug, Serialize)]
pub(crate) struct MarkBlockCompletedRequest {
    pub block_height: u64,
    /// Responds `true` if the block was not previously marked complete.
    pub responder: Responder<bool>,
}

impl Display for MarkBlockCompletedRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "block completed: height {}", self.block_height)
    }
}

#[derive(DataSize, Debug, Serialize)]
pub(crate) enum DeployBufferRequest {
    GetAppendableBlock {
        timestamp: Timestamp,
        responder: Responder<AppendableBlock>,
    },
}

impl Display for DeployBufferRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            DeployBufferRequest::GetAppendableBlock { timestamp, .. } => {
                write!(
                    formatter,
                    "request for appendable block at instant {}",
                    timestamp
                )
            }
        }
    }
}

/// Abstract RPC request.
///
/// An RPC request is an abstract request that does not concern itself with serialization or
/// transport.
#[derive(Debug)]
#[must_use]
pub(crate) enum RpcRequest {
    /// Return transfers for block by hash (if any).
    GetBlockTransfers {
        /// The hash of the block to retrieve transfers for.
        block_hash: BlockHash,
        /// Responder to call with the result.
        responder: Responder<Option<Vec<Transfer>>>,
    },
    /// Query the global state at the given root hash.
    QueryGlobalState {
        /// The state root hash.
        state_root_hash: Digest,
        /// Hex-encoded `casper_types::Key`.
        base_key: Key,
        /// The path components starting from the key as base.
        path: Vec<String>,
        /// Responder to call with the result.
        responder: Responder<Result<QueryResult, engine_state::Error>>,
    },
    /// Query the global state at the given root hash.
    QueryEraValidators {
        /// The global state hash.
        state_root_hash: Digest,
        /// The protocol version.
        protocol_version: ProtocolVersion,
        /// Responder to call with the result.
        responder: Responder<Result<EraValidators, GetEraValidatorsError>>,
    },
    /// Get the bids at the given root hash.
    GetBids {
        /// The global state hash.
        state_root_hash: Digest,
        /// Responder to call with the result.
        responder: Responder<Result<GetBidsResult, engine_state::Error>>,
    },

    /// Query the global state at the given root hash.
    GetBalance {
        /// The state root hash.
        state_root_hash: Digest,
        /// The purse URef.
        purse_uref: URef,
        /// Responder to call with the result.
        responder: Responder<Result<BalanceResult, engine_state::Error>>,
    },
    /// Return the connected peers.
    GetPeers {
        /// Responder to call with the result.
        responder: Responder<BTreeMap<NodeId, String>>,
    },
    /// Return string formatted status or `None` if an error occurred.
    GetStatus {
        /// Responder to call with the result.
        responder: Responder<StatusFeed>,
    },
    /// Return the height range of fully available blocks.
    GetAvailableBlockRange {
        /// Responder to call with the result.
        responder: Responder<AvailableBlockRange>,
    },
}

impl Display for RpcRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            RpcRequest::GetBlockTransfers { block_hash, .. } => {
                write!(formatter, "get transfers {}", block_hash)
            }

            RpcRequest::QueryGlobalState {
                state_root_hash,
                base_key,
                path,
                ..
            } => write!(
                formatter,
                "query {}, base_key: {}, path: {:?}",
                state_root_hash, base_key, path
            ),
            RpcRequest::QueryEraValidators {
                state_root_hash, ..
            } => write!(formatter, "auction {}", state_root_hash),
            RpcRequest::GetBids {
                state_root_hash, ..
            } => {
                write!(formatter, "bids {}", state_root_hash)
            }
            RpcRequest::GetBalance {
                state_root_hash,
                purse_uref,
                ..
            } => write!(
                formatter,
                "balance {}, purse_uref: {}",
                state_root_hash, purse_uref
            ),
            RpcRequest::GetPeers { .. } => write!(formatter, "get peers"),
            RpcRequest::GetStatus { .. } => write!(formatter, "get status"),
            RpcRequest::GetAvailableBlockRange { .. } => {
                write!(formatter, "get available block range")
            }
        }
    }
}

/// Abstract REST request.
///
/// An REST request is an abstract request that does not concern itself with serialization or
/// transport.
#[derive(Debug)]
#[must_use]
pub(crate) enum RestRequest {
    /// Return string formatted status or `None` if an error occurred.
    Status {
        /// Responder to call with the result.
        responder: Responder<StatusFeed>,
    },
    /// Return string formatted, prometheus compatible metrics or `None` if an error occurred.
    Metrics {
        /// Responder to call with the result.
        responder: Responder<Option<String>>,
    },
    /// Returns schema of client-facing JSON-RPCs in OpenRPC format.
    RpcSchema {
        /// Responder to call with the result
        responder: Responder<OpenRpcSchema>,
    },
}

impl Display for RestRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            RestRequest::Status { .. } => write!(formatter, "get status"),
            RestRequest::Metrics { .. } => write!(formatter, "get metrics"),
            RestRequest::RpcSchema { .. } => write!(formatter, "get openrpc"),
        }
    }
}

/// A contract runtime request.
#[derive(Debug, Serialize)]
#[must_use]
pub(crate) enum ContractRuntimeRequest {
    /// A request to enqueue a `ExecutableBlock` for execution.
    EnqueueBlockForExecution {
        /// A `ExecutableBlock` to enqueue.
        executable_block: ExecutableBlock,
        /// The key block height for the current protocol version's activation point.
        key_block_height_for_activation_point: u64,
        meta_block_state: MetaBlockState,
    },
    /// A query request.
    Query {
        /// Query request.
        #[serde(skip_serializing)]
        query_request: QueryRequest,
        /// Responder to call with the query result.
        responder: Responder<Result<QueryResult, engine_state::Error>>,
    },
    /// A balance request.
    GetBalance {
        /// Balance request.
        #[serde(skip_serializing)]
        balance_request: BalanceRequest,
        /// Responder to call with the balance result.
        responder: Responder<Result<BalanceResult, engine_state::Error>>,
    },
    /// Get the total supply on the chain.
    GetTotalSupply {
        #[serde(skip_serializing)]
        total_supply_request: TotalSupplyRequest,
        responder: Responder<Result<U512, engine_state::Error>>,
    },
    /// Get the round seigniorage rate.
    GetRoundSeigniorageRate {
        #[serde(skip_serializing)]
        round_seigniorage_rate_request: RoundSeigniorageRateRequest,
        responder: Responder<Result<Ratio<U512>, engine_state::Error>>,
    },
    /// Returns validator weights.
    GetEraValidators {
        /// Get validators weights request.
        #[serde(skip_serializing)]
        request: EraValidatorsRequest,
        /// Responder to call with the result.
        responder: Responder<Result<EraValidators, GetEraValidatorsError>>,
    },
    /// Return bids at a given state root hash
    GetBids {
        /// Get bids request.
        #[serde(skip_serializing)]
        get_bids_request: GetBidsRequest,
        /// Responder to call with the result.
        responder: Responder<Result<GetBidsResult, engine_state::Error>>,
    },
    /// Returns the value of the execution results checksum stored in the ChecksumRegistry for the
    /// given state root hash.
    GetExecutionResultsChecksum {
        state_root_hash: Digest,
        responder: Responder<Result<Option<Digest>, engine_state::Error>>,
    },
    /// Returns an `AddressableEntity` if found under the given key.  If a legacy `Account` exists
    /// under the given key, it will be converted to an `AddressableEntity` and returned.
    GetAddressableEntity {
        state_root_hash: Digest,
        key: Key,
        responder: Responder<Option<AddressableEntity>>,
    },
    /// Get a trie or chunk by its ID.
    GetTrie {
        /// The ID of the trie (or chunk of a trie) to be read.
        trie_or_chunk_id: TrieOrChunkId,
        /// Responder to call with the result.
        responder: Responder<Result<Option<TrieOrChunk>, ContractRuntimeError>>,
    },
    /// Get a trie by its ID.
    GetTrieFull {
        /// The ID of the trie to be read.
        trie_key: Digest,
        /// Responder to call with the result.
        responder: Responder<Result<Option<Bytes>, engine_state::Error>>,
    },
    /// Insert a trie into global storage
    PutTrie {
        /// The hash of the value to get from the `TrieStore`
        trie_bytes: TrieRaw,
        /// Responder to call with the result. Contains the hash of the stored trie.
        responder: Responder<Result<Digest, engine_state::Error>>,
    },
    /// Execute transaction without committing results
    SpeculativelyExecute {
        /// Hash of a block on top of which to execute the transaction.
        execution_prestate: SpeculativeExecutionState,
        /// Transaction to execute.
        transaction: Box<Transaction>,
        /// Results
        responder: Responder<Result<Option<ExecutionResultV2>, engine_state::Error>>,
    },
}

impl Display for ContractRuntimeRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ContractRuntimeRequest::EnqueueBlockForExecution {
                executable_block, ..
            } => {
                write!(formatter, "executable_block: {}", executable_block)
            }
            ContractRuntimeRequest::Query { query_request, .. } => {
                write!(formatter, "query request: {:?}", query_request)
            }
            ContractRuntimeRequest::GetBalance {
                balance_request, ..
            } => write!(formatter, "balance request: {:?}", balance_request),
            ContractRuntimeRequest::GetTotalSupply {
                total_supply_request,
                ..
            } => {
                write!(formatter, "get total supply: {:?}", total_supply_request)
            }
            ContractRuntimeRequest::GetRoundSeigniorageRate {
                round_seigniorage_rate_request,
                ..
            } => {
                write!(
                    formatter,
                    "get round seigniorage rate: {:?}",
                    round_seigniorage_rate_request
                )
            }
            ContractRuntimeRequest::GetEraValidators { request, .. } => {
                write!(formatter, "get era validators: {:?}", request)
            }
            ContractRuntimeRequest::GetBids {
                get_bids_request, ..
            } => {
                write!(formatter, "get bids request: {:?}", get_bids_request)
            }
            ContractRuntimeRequest::GetExecutionResultsChecksum {
                state_root_hash, ..
            } => write!(
                formatter,
                "get execution results checksum under {}",
                state_root_hash
            ),
            ContractRuntimeRequest::GetAddressableEntity {
                state_root_hash,
                key,
                ..
            } => {
                write!(
                    formatter,
                    "get addressable_entity {} under {}",
                    key, state_root_hash
                )
            }
            ContractRuntimeRequest::GetTrie {
                trie_or_chunk_id, ..
            } => {
                write!(formatter, "get trie_or_chunk_id: {}", trie_or_chunk_id)
            }
            ContractRuntimeRequest::GetTrieFull { trie_key, .. } => {
                write!(formatter, "get trie_key: {}", trie_key)
            }
            ContractRuntimeRequest::PutTrie { trie_bytes, .. } => {
                write!(formatter, "trie: {:?}", trie_bytes)
            }
            ContractRuntimeRequest::SpeculativelyExecute {
                execution_prestate,
                transaction,
                ..
            } => {
                write!(
                    formatter,
                    "Execute {} on {}",
                    transaction.hash(),
                    execution_prestate.state_root_hash
                )
            }
        }
    }
}

/// Fetcher related requests.
#[derive(Debug, Serialize)]
#[must_use]
pub(crate) struct FetcherRequest<T: FetchItem> {
    /// The ID of the item to be retrieved.
    pub(crate) id: T::Id,
    /// The peer id of the peer to be asked if the item is not held locally
    pub(crate) peer: NodeId,
    /// Metadata used during validation of the fetched item.
    pub(crate) validation_metadata: Box<T::ValidationMetadata>,
    /// Responder to call with the result.
    pub(crate) responder: Responder<FetchResult<T>>,
}

impl<T: FetchItem> Display for FetcherRequest<T> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "request item by id {}", self.id)
    }
}

/// TrieAccumulator related requests.
#[derive(Debug, Serialize, DataSize)]
#[must_use]
pub(crate) struct TrieAccumulatorRequest {
    /// The hash of the trie node.
    pub(crate) hash: Digest,
    /// The peers to try to fetch from.
    pub(crate) peers: Vec<NodeId>,
    /// Responder to call with the result.
    pub(crate) responder: Responder<Result<TrieAccumulatorResponse, TrieAccumulatorError>>,
}

impl Display for TrieAccumulatorRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "request trie by hash {}", self.hash)
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct SyncGlobalStateRequest {
    pub(crate) block_hash: BlockHash,
    pub(crate) state_root_hash: Digest,
    #[serde(skip)]
    pub(crate) responder:
        Responder<Result<GlobalStateSynchronizerResponse, GlobalStateSynchronizerError>>,
}

impl Display for SyncGlobalStateRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "request to sync global state at {}",
            self.block_hash
        )
    }
}

/// A block validator request.
#[derive(Debug)]
#[must_use]
pub(crate) struct BlockValidationRequest {
    /// The block to be validated.
    pub(crate) block: ProposedBlock<ClContext>,
    /// The sender of the block, which will be asked to provide all missing deploys.
    pub(crate) sender: NodeId,
    /// Responder to call with the result.
    ///
    /// Indicates whether or not validation was successful.
    pub(crate) responder: Responder<bool>,
}

impl Display for BlockValidationRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let BlockValidationRequest { block, sender, .. } = self;
        write!(f, "validate block {} from {}", block, sender)
    }
}

type BlockHeight = u64;

#[derive(DataSize, Debug)]
#[must_use]
/// Consensus component requests.
pub(crate) enum ConsensusRequest {
    /// Request for our public key, and if we're a validator, the next round length.
    Status(Responder<Option<(PublicKey, Option<TimeDiff>)>>),
    /// Request for a list of validator status changes, by public key.
    ValidatorChanges(Responder<BTreeMap<PublicKey, Vec<(EraId, ValidatorChange)>>>),
}

/// ChainspecLoader component requests.
#[derive(Debug, Serialize)]
pub(crate) enum ChainspecRawBytesRequest {
    /// Request for the chainspec file bytes with the genesis_accounts and global_state bytes, if
    /// they are present.
    GetChainspecRawBytes(Responder<Arc<ChainspecRawBytes>>),
}

impl Display for ChainspecRawBytesRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ChainspecRawBytesRequest::GetChainspecRawBytes(_) => {
                write!(f, "get chainspec raw bytes")
            }
        }
    }
}

/// UpgradeWatcher component request to get the next scheduled upgrade, if any.
#[derive(Debug, Serialize)]
pub(crate) struct UpgradeWatcherRequest(pub(crate) Responder<Option<NextUpgrade>>);

impl Display for UpgradeWatcherRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "get next upgrade")
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ReactorStatusRequest(pub(crate) Responder<(ReactorState, Timestamp)>);

impl Display for ReactorStatusRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "get reactor status")
    }
}

#[derive(Debug, Serialize)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum BlockAccumulatorRequest {
    GetPeersForBlock {
        block_hash: BlockHash,
        responder: Responder<Option<Vec<NodeId>>>,
    },
}

impl Display for BlockAccumulatorRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            BlockAccumulatorRequest::GetPeersForBlock { block_hash, .. } => {
                write!(f, "get peers for {}", block_hash)
            }
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) enum BlockSynchronizerRequest {
    NeedNext,
    DishonestPeers,
    SyncGlobalStates(Vec<(BlockHash, Digest)>),
    Status {
        responder: Responder<BlockSynchronizerStatus>,
    },
}

impl Display for BlockSynchronizerRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            BlockSynchronizerRequest::NeedNext => {
                write!(f, "block synchronizer request: need next")
            }
            BlockSynchronizerRequest::DishonestPeers => {
                write!(f, "block synchronizer request: dishonest peers")
            }
            BlockSynchronizerRequest::Status { .. } => {
                write!(f, "block synchronizer request: status")
            }
            BlockSynchronizerRequest::SyncGlobalStates(_) => {
                write!(f, "request to sync global states")
            }
        }
    }
}

/// A request to set the current shutdown trigger.
#[derive(DataSize, Debug, Serialize)]
pub(crate) struct SetNodeStopRequest {
    /// The specific stop-at spec.
    ///
    /// If `None`, clears the current stop at setting.
    pub(crate) stop_at: Option<StopAtSpec>,
    /// Responder to send the previously set stop-at spec to, if any.
    pub(crate) responder: Responder<Option<StopAtSpec>>,
}

impl Display for SetNodeStopRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.stop_at {
            None => f.write_str("clear node stop"),
            Some(stop_at) => write!(f, "set node stop to: {}", stop_at),
        }
    }
}

/// A request to accept a new transaction.
#[derive(DataSize, Debug, Serialize)]
pub(crate) struct AcceptTransactionRequest {
    pub(crate) transaction: Transaction,
    pub(crate) speculative_exec_at_block: Option<Box<BlockHeader>>,
    pub(crate) responder: Responder<Result<(), transaction_acceptor::Error>>,
}

impl Display for AcceptTransactionRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self.speculative_exec_at_block.is_some() {
            write!(
                f,
                "accept transaction {} for speculative exec",
                self.transaction.hash()
            )
        } else {
            write!(f, "accept transaction {}", self.transaction.hash())
        }
    }
}
