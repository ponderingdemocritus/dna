//! Connect to the sequencer gateway.
use apibara_core::starknet::v1alpha2;
use starknet::{
    core::types::{FieldElement, FromByteArrayError},
    providers::jsonrpc::{self, models::ErrorCode, JsonRpcClientError, RpcError},
};
use url::Url;

use crate::{
    core::{BlockHash, GlobalBlockId, InvalidBlockHashSize},
    db::BlockBody,
};

#[derive(Debug, Clone)]
pub enum BlockId {
    Latest,
    Pending,
    Hash(BlockHash),
    Number(u64),
}

pub trait ProviderError: std::error::Error + Send + Sync + 'static {
    fn is_block_not_found(&self) -> bool;
}

#[apibara_node::async_trait]
pub trait Provider {
    type Error: ProviderError;

    /// Get the most recent accepted block number and hash.
    async fn get_head(&self) -> Result<GlobalBlockId, Self::Error>;

    /// Get a specific block.
    async fn get_block(
        &self,
        id: &BlockId,
    ) -> Result<(v1alpha2::BlockStatus, v1alpha2::BlockHeader, BlockBody), Self::Error>;

    /// Get state update for a specific block.
    async fn get_state_update(&self, id: &BlockId) -> Result<v1alpha2::StateUpdate, Self::Error>;

    /// Get receipt for a specific transaction.
    async fn get_transaction_receipt(
        &self,
        hash: &v1alpha2::FieldElement,
    ) -> Result<v1alpha2::TransactionReceipt, Self::Error>;
}

/// StarkNet RPC provider over HTTP.
pub struct HttpProvider {
    provider: jsonrpc::JsonRpcClient<jsonrpc::HttpTransport>,
}

#[derive(Debug, thiserror::Error)]
pub enum HttpProviderError {
    #[error("the given block was not found")]
    BlockNotFound,
    #[error("failed to parse gateway configuration")]
    Configuration,
    #[error("failed to parse gateway url")]
    Url(#[from] url::ParseError),
    #[error(transparent)]
    Provider(#[from] Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error("received unexpected pending block")]
    UnexpectedPendingBlock,
    #[error("expected pending block, but received non pending block")]
    ExpectedPendingBlock,
    #[error("failed to parse block id")]
    InvalidBlockId(#[from] FromByteArrayError),
    #[error("failed to parse block hash")]
    InvalidBlockHash(#[from] InvalidBlockHashSize),
}

impl HttpProvider {
    pub fn new(rpc_url: Url) -> Self {
        let http = jsonrpc::HttpTransport::new(rpc_url);
        let provider = jsonrpc::JsonRpcClient::new(http);
        HttpProvider { provider }
    }
}

impl ProviderError for HttpProviderError {
    fn is_block_not_found(&self) -> bool {
        matches!(self, HttpProviderError::BlockNotFound)
    }
}

impl HttpProviderError {
    pub fn from_provider_error<T>(error: JsonRpcClientError<T>) -> HttpProviderError
    where
        T: std::error::Error + Send + Sync + 'static,
    {
        match error {
            JsonRpcClientError::RpcError(RpcError::Code(ErrorCode::BlockNotFound)) => {
                HttpProviderError::BlockNotFound
            }
            _ => HttpProviderError::Provider(Box::new(error)),
        }
    }
}

struct TransactionHash<'a>(&'a [u8]);

trait ToProto<T> {
    fn to_proto(&self) -> T;
}

trait TryToProto<T> {
    type Error;

    fn try_to_proto(&self) -> Result<T, Self::Error>;
}

#[apibara_node::async_trait]
impl Provider for HttpProvider {
    type Error = HttpProviderError;

    #[tracing::instrument(skip(self), err(Debug))]
    async fn get_head(&self) -> Result<GlobalBlockId, Self::Error> {
        let hash_and_number = self
            .provider
            .block_hash_and_number()
            .await
            .map_err(HttpProviderError::from_provider_error)?;
        let hash: v1alpha2::FieldElement = hash_and_number.block_hash.into();
        Ok(GlobalBlockId::new(
            hash_and_number.block_number,
            hash.into(),
        ))
    }

    #[tracing::instrument(skip(self), err(Debug))]
    async fn get_block(
        &self,
        id: &BlockId,
    ) -> Result<(v1alpha2::BlockStatus, v1alpha2::BlockHeader, BlockBody), Self::Error> {
        let block_id = id.try_into()?;
        let block = self
            .provider
            .get_block_with_txs(&block_id)
            .await
            .map_err(HttpProviderError::from_provider_error)?;

        match block {
            jsonrpc::models::MaybePendingBlockWithTxs::Block(ref block) => {
                if id.is_pending() {
                    return Err(HttpProviderError::UnexpectedPendingBlock);
                }
                let status = block.to_proto();
                let header = block.to_proto();
                let body = block.to_proto();
                Ok((status, header, body))
            }
            jsonrpc::models::MaybePendingBlockWithTxs::PendingBlock(ref block) => {
                if !id.is_pending() {
                    return Err(HttpProviderError::ExpectedPendingBlock);
                }
                let status = block.to_proto();
                let header = block.to_proto();
                let body = block.to_proto();
                Ok((status, header, body))
            }
        }
    }

    #[tracing::instrument(skip(self), err(Debug))]
    async fn get_state_update(&self, id: &BlockId) -> Result<v1alpha2::StateUpdate, Self::Error> {
        let block_id = id.try_into()?;
        let state_update = self
            .provider
            .get_state_update(&block_id)
            .await
            .map_err(HttpProviderError::from_provider_error)?
            .to_proto();
        Ok(state_update)
    }

    #[tracing::instrument(skip(self), fields(hash = %hash), err(Debug))]
    async fn get_transaction_receipt(
        &self,
        hash: &v1alpha2::FieldElement,
    ) -> Result<v1alpha2::TransactionReceipt, Self::Error> {
        let hash: FieldElement = hash
            .try_into()
            .map_err(|err| HttpProviderError::Provider(Box::new(err)))?;
        let receipt = self
            .provider
            .get_transaction_receipt(hash)
            .await
            .map_err(HttpProviderError::from_provider_error)?
            .to_proto();
        Ok(receipt)
    }
}

impl BlockId {
    pub fn is_pending(&self) -> bool {
        matches!(self, BlockId::Pending)
    }
}

impl TryFrom<&BlockId> for jsonrpc::models::BlockId {
    type Error = FromByteArrayError;

    fn try_from(value: &BlockId) -> Result<Self, Self::Error> {
        use jsonrpc::models::{BlockId as SNBlockId, BlockTag};

        match value {
            BlockId::Latest => Ok(SNBlockId::Tag(BlockTag::Latest)),
            BlockId::Pending => Ok(SNBlockId::Tag(BlockTag::Pending)),
            BlockId::Hash(hash) => {
                let hash = hash.try_into()?;
                Ok(SNBlockId::Hash(hash))
            }
            BlockId::Number(number) => Ok(SNBlockId::Number(*number)),
        }
    }
}

impl ToProto<v1alpha2::BlockStatus> for jsonrpc::models::BlockWithTxs {
    fn to_proto(&self) -> v1alpha2::BlockStatus {
        self.status.to_proto()
    }
}

impl ToProto<v1alpha2::BlockStatus> for jsonrpc::models::PendingBlockWithTxs {
    fn to_proto(&self) -> v1alpha2::BlockStatus {
        v1alpha2::BlockStatus::Pending
    }
}

impl ToProto<v1alpha2::BlockHeader> for jsonrpc::models::BlockWithTxs {
    fn to_proto(&self) -> v1alpha2::BlockHeader {
        let block_hash = self.block_hash.into();
        let parent_block_hash = self.parent_hash.into();
        let block_number = self.block_number;
        let sequencer_address = self.sequencer_address.into();
        let new_root = self.new_root.into();
        let timestamp = pbjson_types::Timestamp {
            nanos: 0,
            seconds: self.timestamp as i64,
        };

        v1alpha2::BlockHeader {
            block_hash: Some(block_hash),
            parent_block_hash: Some(parent_block_hash),
            block_number,
            sequencer_address: Some(sequencer_address),
            new_root: Some(new_root),
            timestamp: Some(timestamp),
        }
    }
}

impl ToProto<v1alpha2::BlockHeader> for jsonrpc::models::PendingBlockWithTxs {
    fn to_proto(&self) -> v1alpha2::BlockHeader {
        let block_hash = FieldElement::ZERO.into();
        let parent_block_hash = self.parent_hash.into();
        let sequencer_address = self.sequencer_address.into();
        let timestamp = pbjson_types::Timestamp {
            nanos: 0,
            seconds: self.timestamp as i64,
        };

        v1alpha2::BlockHeader {
            block_hash: Some(block_hash),
            parent_block_hash: Some(parent_block_hash),
            block_number: u64::MAX,
            sequencer_address: Some(sequencer_address),
            new_root: None,
            timestamp: Some(timestamp),
        }
    }
}

impl ToProto<BlockBody> for jsonrpc::models::BlockWithTxs {
    fn to_proto(&self) -> BlockBody {
        let transactions = self.transactions.iter().map(|tx| tx.to_proto()).collect();
        BlockBody { transactions }
    }
}

impl ToProto<BlockBody> for jsonrpc::models::PendingBlockWithTxs {
    fn to_proto(&self) -> BlockBody {
        let transactions = self.transactions.iter().map(|tx| tx.to_proto()).collect();
        BlockBody { transactions }
    }
}

impl ToProto<v1alpha2::BlockStatus> for jsonrpc::models::BlockStatus {
    fn to_proto(&self) -> v1alpha2::BlockStatus {
        use jsonrpc::models::BlockStatus;

        match self {
            BlockStatus::Pending => v1alpha2::BlockStatus::Pending,
            BlockStatus::AcceptedOnL2 => v1alpha2::BlockStatus::AcceptedOnL2,
            BlockStatus::AcceptedOnL1 => v1alpha2::BlockStatus::AcceptedOnL1,
            BlockStatus::Rejected => v1alpha2::BlockStatus::Rejected,
        }
    }
}

impl ToProto<v1alpha2::Transaction> for jsonrpc::models::Transaction {
    fn to_proto(&self) -> v1alpha2::Transaction {
        use jsonrpc::models::Transaction;

        match self {
            Transaction::Invoke(invoke) => invoke.to_proto(),
            Transaction::Deploy(deploy) => deploy.to_proto(),
            Transaction::Declare(declare) => declare.to_proto(),
            Transaction::L1Handler(l1_handler) => l1_handler.to_proto(),
            Transaction::DeployAccount(deploy_account) => deploy_account.to_proto(),
        }
    }
}

impl ToProto<v1alpha2::Transaction> for jsonrpc::models::InvokeTransaction {
    fn to_proto(&self) -> v1alpha2::Transaction {
        use jsonrpc::models::InvokeTransaction;

        match self {
            InvokeTransaction::V0(v0) => v0.to_proto(),
            InvokeTransaction::V1(v1) => v1.to_proto(),
        }
    }
}

impl ToProto<v1alpha2::Transaction> for jsonrpc::models::InvokeTransactionV0 {
    fn to_proto(&self) -> v1alpha2::Transaction {
        use v1alpha2::transaction::Transaction;

        let hash = self.transaction_hash.into();
        let max_fee = self.max_fee.into();
        let signature = self.signature.iter().map(|fe| fe.into()).collect();
        let nonce = self.nonce.into();

        let meta = v1alpha2::TransactionMeta {
            hash: Some(hash),
            max_fee: Some(max_fee),
            signature,
            nonce: Some(nonce),
            version: 0,
        };

        let contract_address = self.contract_address.into();
        let entry_point_selector = self.entry_point_selector.into();
        let calldata = self.calldata.iter().map(|fe| fe.into()).collect();

        let invoke_v0 = v1alpha2::InvokeTransactionV0 {
            contract_address: Some(contract_address),
            entry_point_selector: Some(entry_point_selector),
            calldata,
        };

        v1alpha2::Transaction {
            meta: Some(meta),
            transaction: Some(Transaction::InvokeV0(invoke_v0)),
        }
    }
}

impl ToProto<v1alpha2::Transaction> for jsonrpc::models::InvokeTransactionV1 {
    fn to_proto(&self) -> v1alpha2::Transaction {
        use v1alpha2::transaction::Transaction;

        let hash = self.transaction_hash.into();
        let max_fee = self.max_fee.into();
        let signature = self.signature.iter().map(|fe| fe.into()).collect();
        let nonce = self.nonce.into();

        let meta = v1alpha2::TransactionMeta {
            hash: Some(hash),
            max_fee: Some(max_fee),
            signature,
            nonce: Some(nonce),
            version: 0,
        };

        let sender_address = self.sender_address.into();
        let calldata = self.calldata.iter().map(|fe| fe.into()).collect();
        let invoke_v1 = v1alpha2::InvokeTransactionV1 {
            sender_address: Some(sender_address),
            calldata,
        };

        v1alpha2::Transaction {
            meta: Some(meta),
            transaction: Some(Transaction::InvokeV1(invoke_v1)),
        }
    }
}

impl ToProto<v1alpha2::Transaction> for jsonrpc::models::DeployTransaction {
    fn to_proto(&self) -> v1alpha2::Transaction {
        use v1alpha2::transaction::Transaction;

        let hash = self.transaction_hash.into();

        let meta = v1alpha2::TransactionMeta {
            hash: Some(hash),
            version: self.version,
            ..v1alpha2::TransactionMeta::default()
        };

        let class_hash = self.class_hash.into();
        let contract_address_salt = self.contract_address_salt.into();
        let constructor_calldata = self
            .constructor_calldata
            .iter()
            .map(|fe| fe.into())
            .collect();

        let deploy = v1alpha2::DeployTransaction {
            class_hash: Some(class_hash),
            contract_address_salt: Some(contract_address_salt),
            constructor_calldata,
        };

        v1alpha2::Transaction {
            meta: Some(meta),
            transaction: Some(Transaction::Deploy(deploy)),
        }
    }
}

impl ToProto<v1alpha2::Transaction> for jsonrpc::models::DeclareTransaction {
    fn to_proto(&self) -> v1alpha2::Transaction {
        use v1alpha2::transaction::Transaction;

        let hash = self.transaction_hash.into();
        let max_fee = self.max_fee.into();
        let signature = self.signature.iter().map(|fe| fe.into()).collect();
        let nonce = self.nonce.into();
        let version = self.version;

        let meta = v1alpha2::TransactionMeta {
            hash: Some(hash),
            max_fee: Some(max_fee),
            signature,
            nonce: Some(nonce),
            version,
        };

        let class_hash = self.class_hash.into();
        let sender_address = self.sender_address.into();

        let declare = v1alpha2::DeclareTransaction {
            class_hash: Some(class_hash),
            sender_address: Some(sender_address),
        };

        v1alpha2::Transaction {
            meta: Some(meta),
            transaction: Some(Transaction::Declare(declare)),
        }
    }
}

impl ToProto<v1alpha2::Transaction> for jsonrpc::models::L1HandlerTransaction {
    fn to_proto(&self) -> v1alpha2::Transaction {
        use v1alpha2::transaction::Transaction;

        let hash = self.transaction_hash.into();
        let version = self.version;

        let meta = v1alpha2::TransactionMeta {
            hash: Some(hash),
            version,
            ..v1alpha2::TransactionMeta::default()
        };

        let contract_address = self.contract_address.into();
        let entry_point_selector = self.entry_point_selector.into();
        let calldata = self.calldata.iter().map(|fe| fe.into()).collect();

        let l1_handler = v1alpha2::L1HandlerTransaction {
            contract_address: Some(contract_address),
            entry_point_selector: Some(entry_point_selector),
            calldata,
        };

        v1alpha2::Transaction {
            meta: Some(meta),
            transaction: Some(Transaction::L1Handler(l1_handler)),
        }
    }
}

impl ToProto<v1alpha2::Transaction> for jsonrpc::models::DeployAccountTransaction {
    fn to_proto(&self) -> v1alpha2::Transaction {
        use v1alpha2::transaction::Transaction;

        let hash = self.transaction_hash.into();
        let max_fee = self.max_fee.into();
        let signature = self.signature.iter().map(|fe| fe.into()).collect();
        let nonce = self.nonce.into();
        let version = self.version;

        let meta = v1alpha2::TransactionMeta {
            hash: Some(hash),
            max_fee: Some(max_fee),
            signature,
            nonce: Some(nonce),
            version,
        };

        let contract_address_salt = self.contract_address_salt.into();
        let class_hash = self.class_hash.into();
        let constructor_calldata = self
            .constructor_calldata
            .iter()
            .map(|fe| fe.into())
            .collect();

        let deploy_account = v1alpha2::DeployAccountTransaction {
            contract_address_salt: Some(contract_address_salt),
            class_hash: Some(class_hash),
            constructor_calldata,
        };

        v1alpha2::Transaction {
            meta: Some(meta),
            transaction: Some(Transaction::DeployAccount(deploy_account)),
        }
    }
}

impl ToProto<v1alpha2::TransactionReceipt> for jsonrpc::models::MaybePendingTransactionReceipt {
    fn to_proto(&self) -> v1alpha2::TransactionReceipt {
        use jsonrpc::models::MaybePendingTransactionReceipt;

        match self {
            MaybePendingTransactionReceipt::PendingReceipt(receipt) => receipt.to_proto(),
            MaybePendingTransactionReceipt::Receipt(receipt) => receipt.to_proto(),
        }
    }
}

impl ToProto<v1alpha2::TransactionReceipt> for jsonrpc::models::PendingTransactionReceipt {
    fn to_proto(&self) -> v1alpha2::TransactionReceipt {
        use jsonrpc::models::PendingTransactionReceipt;

        match self {
            PendingTransactionReceipt::Invoke(invoke) => invoke.to_proto(),
            PendingTransactionReceipt::L1Handler(l1_handler) => l1_handler.to_proto(),
            PendingTransactionReceipt::Declare(declare) => declare.to_proto(),
            PendingTransactionReceipt::Deploy(deploy) => deploy.to_proto(),
            PendingTransactionReceipt::DeployAccount(deploy) => deploy.to_proto(),
        }
    }
}

impl ToProto<v1alpha2::TransactionReceipt> for jsonrpc::models::PendingInvokeTransactionReceipt {
    fn to_proto(&self) -> v1alpha2::TransactionReceipt {
        let transaction_hash = self.transaction_hash.into();
        let actual_fee = self.actual_fee.into();
        let l2_to_l1_messages = self
            .messages_sent
            .iter()
            .map(|msg| msg.to_proto())
            .collect();
        let events = self.events.iter().map(|ev| ev.to_proto()).collect();

        v1alpha2::TransactionReceipt {
            transaction_index: 0,
            transaction_hash: Some(transaction_hash),
            actual_fee: Some(actual_fee),
            l2_to_l1_messages,
            events,
            contract_address: None,
        }
    }
}

impl ToProto<v1alpha2::TransactionReceipt> for jsonrpc::models::PendingL1HandlerTransactionReceipt {
    fn to_proto(&self) -> v1alpha2::TransactionReceipt {
        let transaction_hash = self.transaction_hash.into();
        let actual_fee = self.actual_fee.into();
        let l2_to_l1_messages = self
            .messages_sent
            .iter()
            .map(|msg| msg.to_proto())
            .collect();
        let events = self.events.iter().map(|ev| ev.to_proto()).collect();

        v1alpha2::TransactionReceipt {
            transaction_index: 0,
            transaction_hash: Some(transaction_hash),
            actual_fee: Some(actual_fee),
            l2_to_l1_messages,
            events,
            contract_address: None,
        }
    }
}

impl ToProto<v1alpha2::TransactionReceipt> for jsonrpc::models::PendingDeclareTransactionReceipt {
    fn to_proto(&self) -> v1alpha2::TransactionReceipt {
        let transaction_hash = self.transaction_hash.into();
        let actual_fee = self.actual_fee.into();
        let l2_to_l1_messages = self
            .messages_sent
            .iter()
            .map(|msg| msg.to_proto())
            .collect();
        let events = self.events.iter().map(|ev| ev.to_proto()).collect();

        v1alpha2::TransactionReceipt {
            transaction_index: 0,
            transaction_hash: Some(transaction_hash),
            actual_fee: Some(actual_fee),
            l2_to_l1_messages,
            events,
            contract_address: None,
        }
    }
}

impl ToProto<v1alpha2::TransactionReceipt> for jsonrpc::models::PendingDeployTransactionReceipt {
    fn to_proto(&self) -> v1alpha2::TransactionReceipt {
        let transaction_hash = self.transaction_hash.into();
        let actual_fee = self.actual_fee.into();
        let l2_to_l1_messages = self
            .messages_sent
            .iter()
            .map(|msg| msg.to_proto())
            .collect();
        let events = self.events.iter().map(|ev| ev.to_proto()).collect();
        let contract_address = self.contract_address.into();

        v1alpha2::TransactionReceipt {
            transaction_index: 0,
            transaction_hash: Some(transaction_hash),
            actual_fee: Some(actual_fee),
            l2_to_l1_messages,
            events,
            contract_address: Some(contract_address),
        }
    }
}

impl ToProto<v1alpha2::TransactionReceipt>
    for jsonrpc::models::PendingDeployAccountTransactionReceipt
{
    fn to_proto(&self) -> v1alpha2::TransactionReceipt {
        let transaction_hash = self.transaction_hash.into();
        let actual_fee = self.actual_fee.into();
        let l2_to_l1_messages = self
            .messages_sent
            .iter()
            .map(|msg| msg.to_proto())
            .collect();
        let events = self.events.iter().map(|ev| ev.to_proto()).collect();

        v1alpha2::TransactionReceipt {
            transaction_index: 0,
            transaction_hash: Some(transaction_hash),
            actual_fee: Some(actual_fee),
            l2_to_l1_messages,
            events,
            contract_address: None,
        }
    }
}

impl ToProto<v1alpha2::TransactionReceipt> for jsonrpc::models::TransactionReceipt {
    fn to_proto(&self) -> v1alpha2::TransactionReceipt {
        use jsonrpc::models::TransactionReceipt;

        match self {
            TransactionReceipt::Invoke(invoke) => invoke.to_proto(),
            TransactionReceipt::L1Handler(l1_handler) => l1_handler.to_proto(),
            TransactionReceipt::Declare(declare) => declare.to_proto(),
            TransactionReceipt::Deploy(deploy) => deploy.to_proto(),
            TransactionReceipt::DeployAccount(deploy) => deploy.to_proto(),
        }
    }
}

impl ToProto<v1alpha2::TransactionReceipt> for jsonrpc::models::InvokeTransactionReceipt {
    fn to_proto(&self) -> v1alpha2::TransactionReceipt {
        let transaction_hash = self.transaction_hash.into();
        let actual_fee = self.actual_fee.into();
        let l2_to_l1_messages = self
            .messages_sent
            .iter()
            .map(|msg| msg.to_proto())
            .collect();
        let events = self.events.iter().map(|ev| ev.to_proto()).collect();

        v1alpha2::TransactionReceipt {
            transaction_index: 0,
            transaction_hash: Some(transaction_hash),
            actual_fee: Some(actual_fee),
            l2_to_l1_messages,
            events,
            contract_address: None,
        }
    }
}

impl ToProto<v1alpha2::TransactionReceipt> for jsonrpc::models::L1HandlerTransactionReceipt {
    fn to_proto(&self) -> v1alpha2::TransactionReceipt {
        let transaction_hash = self.transaction_hash.into();
        let actual_fee = self.actual_fee.into();
        let l2_to_l1_messages = self
            .messages_sent
            .iter()
            .map(|msg| msg.to_proto())
            .collect();
        let events = self.events.iter().map(|ev| ev.to_proto()).collect();

        v1alpha2::TransactionReceipt {
            transaction_index: 0,
            transaction_hash: Some(transaction_hash),
            actual_fee: Some(actual_fee),
            l2_to_l1_messages,
            events,
            contract_address: None,
        }
    }
}

impl ToProto<v1alpha2::TransactionReceipt> for jsonrpc::models::DeclareTransactionReceipt {
    fn to_proto(&self) -> v1alpha2::TransactionReceipt {
        let transaction_hash = self.transaction_hash.into();
        let actual_fee = self.actual_fee.into();
        let l2_to_l1_messages = self
            .messages_sent
            .iter()
            .map(|msg| msg.to_proto())
            .collect();
        let events = self.events.iter().map(|ev| ev.to_proto()).collect();

        v1alpha2::TransactionReceipt {
            transaction_index: 0,
            transaction_hash: Some(transaction_hash),
            actual_fee: Some(actual_fee),
            l2_to_l1_messages,
            events,
            contract_address: None,
        }
    }
}

impl ToProto<v1alpha2::TransactionReceipt> for jsonrpc::models::DeployTransactionReceipt {
    fn to_proto(&self) -> v1alpha2::TransactionReceipt {
        let transaction_hash = self.transaction_hash.into();
        let actual_fee = self.actual_fee.into();
        let l2_to_l1_messages = self
            .messages_sent
            .iter()
            .map(|msg| msg.to_proto())
            .collect();
        let events = self.events.iter().map(|ev| ev.to_proto()).collect();
        let contract_address = self.contract_address.into();

        v1alpha2::TransactionReceipt {
            transaction_index: 0,
            transaction_hash: Some(transaction_hash),
            actual_fee: Some(actual_fee),
            l2_to_l1_messages,
            events,
            contract_address: Some(contract_address),
        }
    }
}

impl ToProto<v1alpha2::TransactionReceipt> for jsonrpc::models::DeployAccountTransactionReceipt {
    fn to_proto(&self) -> v1alpha2::TransactionReceipt {
        let transaction_hash = self.transaction_hash.into();
        let actual_fee = self.actual_fee.into();
        let l2_to_l1_messages = self
            .messages_sent
            .iter()
            .map(|msg| msg.to_proto())
            .collect();
        let events = self.events.iter().map(|ev| ev.to_proto()).collect();
        let contract_address = self.contract_address.into();

        v1alpha2::TransactionReceipt {
            transaction_index: 0,
            transaction_hash: Some(transaction_hash),
            actual_fee: Some(actual_fee),
            l2_to_l1_messages,
            events,
            contract_address: Some(contract_address),
        }
    }
}

impl ToProto<v1alpha2::L2ToL1Message> for jsonrpc::models::MsgToL1 {
    fn to_proto(&self) -> v1alpha2::L2ToL1Message {
        let to_address = self.to_address.into();
        let payload = self.payload.iter().map(|p| p.into()).collect();

        v1alpha2::L2ToL1Message {
            to_address: Some(to_address),
            payload,
        }
    }
}

impl ToProto<v1alpha2::Event> for jsonrpc::models::Event {
    fn to_proto(&self) -> v1alpha2::Event {
        let from_address = self.from_address.into();
        let keys = self.keys.iter().map(|k| k.into()).collect();
        let data = self.data.iter().map(|d| d.into()).collect();

        v1alpha2::Event {
            from_address: Some(from_address),
            keys,
            data,
        }
    }
}

impl<'a> TryFrom<TransactionHash<'a>> for FieldElement {
    type Error = FromByteArrayError;

    fn try_from(value: TransactionHash<'a>) -> Result<Self, Self::Error> {
        let hash_len = value.0.len();
        if hash_len > 32 {
            return Err(FromByteArrayError);
        }
        let mut buf = [0; 32];
        buf[..hash_len].copy_from_slice(value.0);
        FieldElement::from_bytes_be(&buf)
    }
}

impl ToProto<v1alpha2::StateUpdate> for jsonrpc::models::StateUpdate {
    fn to_proto(&self) -> v1alpha2::StateUpdate {
        let new_root = self.new_root.into();
        let old_root = self.old_root.into();
        let state_diff = self.state_diff.to_proto();
        v1alpha2::StateUpdate {
            new_root: Some(new_root),
            old_root: Some(old_root),
            state_diff: Some(state_diff),
        }
    }
}

impl ToProto<v1alpha2::StateDiff> for jsonrpc::models::StateDiff {
    fn to_proto(&self) -> v1alpha2::StateDiff {
        let storage_diffs = self.storage_diffs.iter().map(|d| d.to_proto()).collect();
        let declared_contracts = self
            .declared_contract_hashes
            .iter()
            .map(|d| d.to_proto())
            .collect();
        let deployed_contracts = self
            .deployed_contracts
            .iter()
            .map(|d| d.to_proto())
            .collect();
        let nonces = self.nonces.iter().map(|d| d.to_proto()).collect();
        v1alpha2::StateDiff {
            storage_diffs,
            declared_contracts,
            deployed_contracts,
            nonces,
        }
    }
}

impl ToProto<v1alpha2::StorageDiff> for jsonrpc::models::ContractStorageDiffItem {
    fn to_proto(&self) -> v1alpha2::StorageDiff {
        let contract_address = self.address.into();
        let storage_entries = self.storage_entries.iter().map(|e| e.to_proto()).collect();
        v1alpha2::StorageDiff {
            contract_address: Some(contract_address),
            storage_entries,
        }
    }
}

impl ToProto<v1alpha2::StorageEntry> for jsonrpc::models::StorageEntry {
    fn to_proto(&self) -> v1alpha2::StorageEntry {
        let key = self.key.into();
        let value = self.value.into();
        v1alpha2::StorageEntry {
            key: Some(key),
            value: Some(value),
        }
    }
}

impl ToProto<v1alpha2::DeclaredContract> for FieldElement {
    fn to_proto(&self) -> v1alpha2::DeclaredContract {
        let class_hash = self.into();
        v1alpha2::DeclaredContract {
            class_hash: Some(class_hash),
        }
    }
}

impl ToProto<v1alpha2::DeployedContract> for jsonrpc::models::DeployedContractItem {
    fn to_proto(&self) -> v1alpha2::DeployedContract {
        let contract_address = self.address.into();
        let class_hash = self.class_hash.into();
        v1alpha2::DeployedContract {
            contract_address: Some(contract_address),
            class_hash: Some(class_hash),
        }
    }
}

impl ToProto<v1alpha2::NonceUpdate> for jsonrpc::models::NonceUpdate {
    fn to_proto(&self) -> v1alpha2::NonceUpdate {
        let contract_address = self.contract_address.into();
        let nonce = self.nonce.into();
        v1alpha2::NonceUpdate {
            contract_address: Some(contract_address),
            nonce: Some(nonce),
        }
    }
}
