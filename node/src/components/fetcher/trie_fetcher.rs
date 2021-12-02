use std::{
    collections::HashMap,
    fmt::{self, Debug},
};

use datasize::DataSize;
use derive_more::From;
use tracing::{debug, error, warn};

use casper_execution_engine::storage::trie::{Trie, TrieOrChunkedData, TrieOrChunkedDataId};
use casper_hashing::{ChunkWithProof, Digest};
use casper_types::{bytesrepr, Key, StoredValue};

use crate::{
    components::{
        fetcher::{event::FetchResult, ReactorEventT},
        Component,
    },
    effect::{
        requests::{FetcherRequest, TrieFetcherRequest},
        EffectBuilder, EffectExt, Effects, Responder,
    },
    types::Item,
    NodeRng,
};

#[derive(DataSize, Debug)]
pub(crate) struct PartialChunks<I> {
    peers: Vec<I>,
    responder: Responder<Option<Trie<Key, StoredValue>>>,
    chunks: HashMap<u64, ChunkWithProof>,
}

impl<I> PartialChunks<I> {
    fn missing_chunk(&self, count: u64) -> Option<u64> {
        (0..count).find(|idx| !self.chunks.contains_key(idx))
    }

    fn assemble_chunks(&self, count: u64) -> Result<Trie<Key, StoredValue>, bytesrepr::Error> {
        let data: Vec<u8> = (0..count)
            .filter_map(|index| self.chunks.get(&index))
            .flat_map(|chunk| chunk.chunk())
            .copied()
            .collect();
        bytesrepr::deserialize(data)
    }
}

#[derive(DataSize, Debug)]
pub(crate) struct TrieFetcher<I> {
    partial_chunks: HashMap<Digest, PartialChunks<I>>,
}

#[derive(DataSize, Debug, From)]
pub(crate) enum Event<I> {
    #[from]
    Request(TrieFetcherRequest<I>),
    TrieOrChunkFetched {
        id: TrieOrChunkedDataId,
        maybe_fetch_result: Option<FetchResult<TrieOrChunkedData, I>>,
    },
}

impl<I> fmt::Display for Event<I> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Event::Request(_) => write!(f, "trie fetcher request"),
            Event::TrieOrChunkFetched { id, .. } => write!(f, "trie or chunk {} fetched", id),
        }
    }
}

impl<I> TrieFetcher<I>
where
    I: Debug + Clone + Send + 'static,
{
    pub(crate) fn new() -> Self {
        TrieFetcher {
            partial_chunks: Default::default(),
        }
    }

    fn consume_trie_or_chunk<REv>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        sender: Option<I>,
        trie_or_chunk: TrieOrChunkedData,
    ) -> Effects<Event<I>>
    where
        REv: ReactorEventT<TrieOrChunkedData> + From<FetcherRequest<I, TrieOrChunkedData>>,
    {
        let TrieOrChunkedDataId(_index, hash) = trie_or_chunk.id();
        match trie_or_chunk {
            TrieOrChunkedData::Trie(trie) => match self.partial_chunks.remove(&hash) {
                None => {
                    error!("fetched a trie we didn't request! {}", hash);
                    Effects::new()
                }
                Some(partial_chunks) => {
                    debug!("got a full trie with hash {}", hash);
                    partial_chunks.responder.respond(Some(trie)).ignore()
                }
            },
            TrieOrChunkedData::ChunkWithProof(chunk) => {
                self.consume_chunk(effect_builder, sender, chunk)
            }
        }
    }

    fn consume_chunk<REv>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        sender: Option<I>,
        chunk: ChunkWithProof,
    ) -> Effects<Event<I>>
    where
        REv: ReactorEventT<TrieOrChunkedData> + From<FetcherRequest<I, TrieOrChunkedData>>,
    {
        if !chunk.verify() {
            match sender {
                None => {
                    error!("got an invalid chunk from storage! {:?}", chunk);
                    return Effects::new();
                }
                Some(sender) => {
                    warn!("got an invalid chunk from {:?}! {:?}", sender, chunk);
                    // TODO: would be good to re-request from someone else instead of the same
                    // node...
                    let id = TrieOrChunkedDataId(chunk.proof().index(), chunk.proof().root_hash());
                    return effect_builder.fetch_trie_or_chunk(id, sender).event(
                        move |maybe_fetch_result| Event::TrieOrChunkFetched {
                            id,
                            maybe_fetch_result,
                        },
                    );
                }
            }
        }
        let digest = chunk.proof().root_hash();
        let index = chunk.proof().index();
        let count = chunk.proof().count();
        let mut partial_chunks = match self.partial_chunks.remove(&digest) {
            None => {
                error!("got a chunk that wasn't requested! {},{}", digest, index);
                return Effects::new();
            }
            Some(partial_chunks) => partial_chunks,
        };

        // Add the downloaded chunk to cache.
        let _ = partial_chunks.chunks.insert(index, chunk);

        // Check if we can now return a complete trie.
        match partial_chunks.missing_chunk(count) {
            Some(missing_index) => {
                let peer = match partial_chunks.peers.last() {
                    Some(peer) => peer.clone(),
                    None => {
                        debug!(
                            "no peers to download the next chunk {},{} from! giving up",
                            digest, missing_index
                        );
                        return partial_chunks.responder.respond(None).ignore();
                    }
                };
                let next_id = TrieOrChunkedDataId(missing_index, digest);
                self.try_download_chunk(effect_builder, next_id, peer, partial_chunks)
            }
            None => match partial_chunks.assemble_chunks(count) {
                Ok(trie) => partial_chunks.responder.respond(Some(trie)).ignore(),
                Err(error) => {
                    error!(
                        "error while assembling a complete trie {}: {}",
                        digest, error
                    );
                    partial_chunks.responder.respond(None).ignore()
                }
            },
        }
    }

    fn try_download_chunk<REv>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        id: TrieOrChunkedDataId,
        peer: I,
        partial_chunks: PartialChunks<I>,
    ) -> Effects<Event<I>>
    where
        REv: ReactorEventT<TrieOrChunkedData> + From<FetcherRequest<I, TrieOrChunkedData>>,
    {
        let TrieOrChunkedDataId(_, hash) = id;
        let _ = self.partial_chunks.insert(hash, partial_chunks);
        effect_builder
            .fetch_trie_or_chunk(id, peer)
            .event(move |maybe_fetch_result| Event::TrieOrChunkFetched {
                id,
                maybe_fetch_result,
            })
    }
}

impl<I, REv> Component<REv> for TrieFetcher<I>
where
    REv: ReactorEventT<TrieOrChunkedData> + From<FetcherRequest<I, TrieOrChunkedData>>,
    I: Debug + Clone + Send + 'static,
{
    type Event = Event<I>;
    type ConstructionError = prometheus::Error;

    fn handle_event(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        _rng: &mut NodeRng,
        event: Self::Event,
    ) -> Effects<Self::Event> {
        debug!(?event, "handling event");
        match event {
            Event::Request(TrieFetcherRequest {
                hash,
                responder,
                peers,
            }) => {
                let trie_id = TrieOrChunkedDataId(0, hash);
                let peer = match peers.last() {
                    Some(peer) => peer.clone(),
                    None => {
                        error!("tried to fetch trie {} with no peers available", hash);
                        return Effects::new();
                    }
                };
                let partial_chunks = PartialChunks {
                    responder,
                    peers,
                    chunks: Default::default(),
                };
                self.try_download_chunk(effect_builder, trie_id, peer, partial_chunks)
            }
            Event::TrieOrChunkFetched {
                id,
                maybe_fetch_result,
            } => {
                let TrieOrChunkedDataId(_index, hash) = id;
                match maybe_fetch_result {
                    None => match self.partial_chunks.remove(&hash) {
                        None => {
                            error!(
                                "got a fetch result for a chunk we weren't trying to \
                                    fetch! {}",
                                id
                            );
                            Effects::new()
                        }
                        Some(mut partial_chunks) => {
                            // remove the last peer from eligible peers
                            let _ = partial_chunks.peers.pop();
                            // try with the next one, if possible
                            match partial_chunks.peers.last().cloned() {
                                Some(next_peer) => self.try_download_chunk(
                                    effect_builder,
                                    id,
                                    next_peer,
                                    partial_chunks,
                                ),
                                None => {
                                    debug!("couldn't fetch chunk {}", id);
                                    partial_chunks.responder.respond(None).ignore()
                                }
                            }
                        }
                    },
                    Some(FetchResult::FromStorage(trie_or_chunk)) => {
                        debug!("got trie or chunk {} from storage", trie_or_chunk);
                        self.consume_trie_or_chunk(effect_builder, None, *trie_or_chunk)
                    }
                    Some(FetchResult::FromPeer(trie_or_chunk, peer)) => {
                        debug!("got trie or chunk {} from {:?}", trie_or_chunk, peer);
                        self.consume_trie_or_chunk(effect_builder, Some(peer), *trie_or_chunk)
                    }
                }
            }
        }
    }
}
