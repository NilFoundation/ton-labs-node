use crate::{
    block::{
        compare_block_ids, convert_block_id_ext_api2blk, convert_block_id_ext_blk2api, BlockStuff
    },
    block_proof::BlockProofStuff, shard_state::ShardStateStuff,
    network::neighbours::{Neighbours, Neighbour},
    types::top_block_descr::TopBlockDescrStuff,
};

use adnl::{common::{serialize, serialize_append}, node::AdnlNode};
use overlay::{OverlayShortId, OverlayNode};
use rldp::RldpNode;
use std::{io::Cursor, time::{Duration, Instant}, sync::Arc};
use ton_api::{BoxedSerialize, BoxedDeserialize, Deserializer, IntoBoxed};
use ton_api::ton::{
    self, TLObject,
    rpc::{
        ton_node::{
            DownloadNextBlockFull, DownloadPersistentStateSlice, DownloadZeroState,
            PreparePersistentState, DownloadBlockProof, DownloadBlockProofLink,
            DownloadKeyBlockProof, DownloadKeyBlockProofLink, PrepareBlock, DownloadBlockFull,
            PrepareZeroState, GetNextKeyBlockIds, GetArchiveInfo, GetArchiveSlice
        }
    },
    ton_node::{ 
        ArchiveInfo, Broadcast, 
        Broadcast::{TonNode_BlockBroadcast, TonNode_NewShardBlockBroadcast}, 
        DataFull, KeyBlocks, Prepared, PreparedProof, PreparedState, 
        broadcast::{BlockBroadcast, ExternalMessageBroadcast, NewShardBlockBroadcast}, 
        externalmessage::ExternalMessage, 
    }
};
use ton_block::BlockIdExt;
use ton_types::{fail, error, Result};

#[async_trait::async_trait]
pub trait FullNodeOverlayClient : Sync + Send {
    async fn broadcast_external_message(&self, msg: &[u8]) -> Result<u32>;
    async fn send_block_broadcast(&self, broadcast: BlockBroadcast) -> Result<()>;
    async fn send_top_shard_block_description(&self, tbd: &TopBlockDescrStuff) -> Result<()>;
    async fn download_block_proof(&self, block_id: &BlockIdExt, is_link: bool, key_block: bool) -> Result<Option<BlockProofStuff>>;
    async fn download_block_full(&self, id: &BlockIdExt) -> Result<Option<(BlockStuff, BlockProofStuff)>>;
    async fn check_persistent_state(
        &self,
        block_id: &BlockIdExt,
        masterchain_block_id: &BlockIdExt,
    ) -> Result<(bool, Arc<Neighbour>)>;
    async fn download_persistent_state_part(
        &self,
        block_id: &BlockIdExt,
        masterchain_block_id: &BlockIdExt,
        offset: usize,
        max_size: usize,
        peer: Arc<Neighbour>,
        attempt: u32,
    ) -> Result<Vec<u8>>;
    async fn download_zero_state(&self, id: &BlockIdExt) -> Result<Option<(ShardStateStuff, Vec<u8>)>>;
    async fn download_next_key_blocks_ids(&self, block_id: &BlockIdExt, max_size: i32) -> Result<Vec<BlockIdExt>>;
    async fn download_next_block_full(&self, prev_id: &BlockIdExt) -> Result<Option<(BlockStuff, BlockProofStuff)>>;
    async fn download_archive(&self, mc_seq_no: u32) -> Result<Option<Vec<u8>>>;

    async fn wait_broadcast(&self) -> Result<Broadcast>;
}

#[derive(Clone)]
pub struct NodeClientOverlay {
    overlay_id: Arc<OverlayShortId>,
    overlay: Arc<OverlayNode>,
    rldp: Arc<RldpNode>,
    peers: Arc<Neighbours>
}

impl NodeClientOverlay {

    const ADNL_ATTEMPTS: u32 = 50;
    const TIMEOUT_PREPARE: u64 = 6000; // Milliseconds
    const TIMEOUT_DELTA: u64 = 50;     // Milliseconds

    pub fn new(
        overlay_id: Arc<OverlayShortId>,
        overlay: Arc<OverlayNode>,
        rldp: Arc<RldpNode>,
        peers: Arc<Neighbours>
    ) -> Self {
        Self{overlay_id, overlay, rldp, peers}
    }

    pub fn overlay_id(&self) -> &Arc<OverlayShortId> {
        &self.overlay_id
    }

/*
    pub fn overlay(&self) -> &Arc<OverlayNode> {
        &self.overlay
    }
*/

    pub fn peers(&self) -> &Arc<Neighbours> {
        &self.peers
    }

    // use this function if request size and answer size < 768 bytes (send query via ADNL)
    async fn send_adnl_query<T, D>(
        &self, 
        request: T, 
        attempts: Option<u32>,
        timeout: Option<u64>
    ) -> Result<(D, Arc<Neighbour>)>
    where
        T: ton_api::AnyBoxedSerialize,
        D: ton_api::AnyBoxedSerialize
    {

        let data = TLObject::new(request);
        let attempts = attempts.unwrap_or(Self::ADNL_ATTEMPTS);

        for _ in 0..attempts {

            let peer = self.peers.choose_neighbour()?.ok_or_else(||error!("neighbour is not found!"))?;
            log::trace!("USE PEER {}, REQUEST {:?}", peer.id(), data);

            let now = std::time::Instant::now();
            let timeout = timeout.or(Some(AdnlNode::calc_timeout(peer.roundtrip_adnl())));
            let answer = self.overlay.query(peer.id(), &data, &self.overlay_id, timeout).await?;
            let t = now.elapsed();
         
            if let Some(answer) = answer {
                match answer.downcast::<D>() {
                    Ok(answer) => {
                        self.peers.update_neighbour_stats(peer.id(), &t, true, false, false)?;
                        return Ok((answer, peer))
                    },
                    Err(obj) => {
                        log::warn!("Wrong answer {:?} to {:?} from {}", obj, data, peer.id())
                    }
                }
            } else {
                log::warn!("No reply to {:?} from {}", data, peer.id())
            }
            self.peers.update_neighbour_stats(peer.id(), &t, false, false, true)?;

        }

        fail!("Cannot send query {:?} in {} attempts", data, attempts)
 
    }

    async fn send_rldp_query_raw<T>(
        &self, 
        request: &T,
        peer: Arc<Neighbour>,
        attempt: u32,
    ) -> Result<Vec<u8>>
    where
        T: BoxedSerialize + std::fmt::Debug
    {
        let (answer, peer, elapsed) = self.send_rldp_query(request, peer, attempt).await?;
        self.peers.update_neighbour_stats(peer.id(), &elapsed, true, true, true)?;
        Ok(answer)
    }

    async fn send_rldp_query_typed<T, D>(
        &self,
        request: &T,
        peer: Arc<Neighbour>,
        attempt: u32,
    ) -> Result<D>
    where
        T: BoxedSerialize + std::fmt::Debug,
        D: BoxedDeserialize
    {
        let (answer, peer, elapsed) = self.send_rldp_query(request, peer, attempt).await?;
        match Deserializer::new(&mut Cursor::new(answer)).read_boxed() {
            Ok(data) => {
                self.peers.update_neighbour_stats(peer.id(), &elapsed, true, true, true)?;
                Ok(data)
            },
            Err(e) => {
                self.peers.update_neighbour_stats(peer.id(), &elapsed, false, true, true)?;
                fail!(e)
            }
        }
    }

    async fn send_rldp_query<T>(
        &self, 
        request: &T, 
        peer: Arc<Neighbour>,
        attempt: u32
    ) -> Result<(Vec<u8>, Arc<Neighbour>, Duration)> 
    where 
        T: BoxedSerialize + std::fmt::Debug 
    {
        let mut query = self.overlay.get_query_prefix(&self.overlay_id)?;
        serialize_append(&mut query, request)?;
        let data = Arc::new(query);

        log::trace!("USE PEER {}, REQUEST {:?}", peer.id(), request);
        let now = Instant::now();
        let answer = self.overlay.query_via_rldp(
            &self.rldp,
            peer.id(),
            &data,
            Some(10 * 1024 * 1024),
            peer.roundtrip_adnl(),
            peer.roundtrip_rldp().map(|t| t + attempt as u64 * Self::TIMEOUT_DELTA),
            &self.overlay_id
        ).await?;

        let elapsed = now.elapsed();
        if let Some(answer) = answer {
            Ok((answer, peer, elapsed))
        } else {
            self.peers.update_neighbour_stats(peer.id(), &elapsed, false, true, true)?;
            fail!("No RLDP answer to {:?} from {}", request, peer.id())
        }
    }
}

#[async_trait::async_trait]
impl FullNodeOverlayClient for NodeClientOverlay {

    // Returns number of nodes to broadcast to
    async fn broadcast_external_message(&self, msg: &[u8]) -> Result<u32>{
        let broadcast = ExternalMessageBroadcast {
            message: ExternalMessage {
                data: ton::bytes(msg.to_vec())
            }
        }.into_boxed();
        self.overlay.broadcast(&self.overlay_id, &serialize(&broadcast)?, None).await
    }

    async fn send_block_broadcast(&self, broadcast: BlockBroadcast) -> Result<()> {
        let id = broadcast.id.clone();
        let block_bloadcast = TonNode_BlockBroadcast(Box::new(broadcast));
        let n = self.overlay.broadcast(&self.overlay_id, &serialize(&block_bloadcast)?, None).await?;
        log::trace!("send_block_broadcast {} (overlay {}) sent to {} nodes", self.overlay_id, id, n);
        Ok(())
    }
    
    async fn send_top_shard_block_description(&self, tbd: &TopBlockDescrStuff) -> Result<()> {
        let broadcast = TonNode_NewShardBlockBroadcast(Box::new(
            NewShardBlockBroadcast { block: tbd.new_shard_block()? })
        );
        let n = self.overlay.broadcast(&self.overlay_id, &serialize(&broadcast)?, None).await?;
        log::trace!("send_top_shard_block_description {} (overlay {}) sent to {} nodes", tbd.proof_for(), self.overlay_id, n);
        Ok(())
    }

    // tonNode.prepareBlockProof block:tonNode.blockIdExt allow_partial:Bool = tonNode.PreparedProof;
    // tonNode.preparedProofEmpty = tonNode.PreparedProof;
    // tonNode.preparedProof = tonNode.PreparedProof;
    // tonNode.preparedProofLink = tonNode.PreparedProof;
    //
    // tonNode.downloadBlockProof block:tonNode.blockIdExt = tonNode.Data;
    // tonNode.downloadBlockProofLink block:tonNode.blockIdExt = tonNode.Data;
    async fn download_block_proof(
        &self, 
        block_id: &BlockIdExt, 
        is_link: bool, 
        key_block: bool, 
    ) -> Result<Option<BlockProofStuff>> {
        // Prepare
        let (prepare, good_peer): (PreparedProof, _) = if key_block {
            self.send_adnl_query(
                ton_api::ton::rpc::ton_node::PrepareKeyBlockProof {
                    block: convert_block_id_ext_blk2api(block_id),
                    allow_partial: is_link.into()
                },
                None,
                Some(Self::TIMEOUT_PREPARE)
            ).await?
        } else {
            self.send_adnl_query(
                ton_api::ton::rpc::ton_node::PrepareBlockProof {
                    block: convert_block_id_ext_blk2api(block_id),
                    allow_partial: is_link.into(),
                },
                None,
                Some(Self::TIMEOUT_PREPARE)
            ).await?
        };

        // Download
        match prepare {
            PreparedProof::TonNode_PreparedProofEmpty => Ok(None),
            PreparedProof::TonNode_PreparedProof => Ok(Some(if key_block {
                BlockProofStuff::deserialize(block_id, self.send_rldp_query_raw(
                    &DownloadKeyBlockProof { block: convert_block_id_ext_blk2api(block_id) },
                    good_peer,
                    0
                ).await?, false)?
            } else {
                BlockProofStuff::deserialize(block_id, self.send_rldp_query_raw(
                    &DownloadBlockProof { block: convert_block_id_ext_blk2api(block_id), },
                    good_peer,
                    0
                ).await?, false)?
            })),
            PreparedProof::TonNode_PreparedProofLink => Ok(Some(if key_block {
                BlockProofStuff::deserialize(block_id, self.send_rldp_query_raw(
                    &DownloadKeyBlockProofLink { block: convert_block_id_ext_blk2api(block_id), },
                    good_peer,
                    0
                ).await?, true)?
            } else {
                BlockProofStuff::deserialize(block_id, self.send_rldp_query_raw(
                    &DownloadBlockProofLink { block: convert_block_id_ext_blk2api(block_id), },
                    good_peer,
                    0
                ).await?, true)?
            }))
        }
    }

    // tonNode.prepareBlock block:tonNode.blockIdExt = tonNode.Prepared;
    // tonNode.downloadBlockFull block:tonNode.blockIdExt = tonNode.DataFull;
    // tonNode.dataFull id:tonNode.blockIdExt proof:bytes block:bytes is_link:Bool = tonNode.DataFull;
    // tonNode.dataFullEmpty = tonNode.DataFull;
    //
    // tonNode.downloadBlock block:tonNode.blockIdExt = tonNode.Data; DEPRECATED?
    async fn download_block_full(
        &self, 
        id: &BlockIdExt, 
    ) -> Result<Option<(BlockStuff, BlockProofStuff)>> {
        // Prepare
        let (prepare, peer): (Prepared, _) = self.send_adnl_query(
            PrepareBlock {block: convert_block_id_ext_blk2api(id)},
            Some(1),
            None
        ).await?;
        log::trace!("USE PEER {}, PREPARE {} FINISHED", peer.id(), id);

        // Download
        match prepare {
            Prepared::TonNode_NotFound => Ok(None),
            Prepared::TonNode_Prepared => {
                let data_full: DataFull = self.send_rldp_query_typed(
                    &DownloadBlockFull {
                        block: convert_block_id_ext_blk2api(id),
                    },
                    peer,
                    0,
                ).await?;

                match data_full {
                    DataFull::TonNode_DataFullEmpty => {
                        log::warn!("prepareBlock receives Prepared, but DownloadBlockFull receives DataFullEmpty");
                        Ok(None)
                    },
                    DataFull::TonNode_DataFull(data_full) => {
                        if !compare_block_ids(&id, &data_full.id) {
                            fail!("Block with another id was received");
                        }

                        let block = BlockStuff::deserialize_checked(id.clone(), data_full.block.0)?;
                        let proof = BlockProofStuff::deserialize(
                            block.id(),
                            data_full.proof.0,
                            data_full.is_link.into())?;

                        Ok(Some((block, proof)))
                    }
                }
            }
        }
    }

    async fn check_persistent_state(
        &self,
        block_id: &BlockIdExt,
        masterchain_block_id: &BlockIdExt,
    ) -> Result<(bool, Arc<Neighbour>)> {
        let (prepare, peer): (PreparedState, _) = self.send_adnl_query(
            TLObject::new(PreparePersistentState {
                block: convert_block_id_ext_blk2api(block_id),
                masterchain_block: convert_block_id_ext_blk2api(masterchain_block_id)
            }),
            None,
            Some(Self::TIMEOUT_PREPARE)
        ).await?;

        Ok((
            match prepare {
                PreparedState::TonNode_NotFoundState => false,
                PreparedState::TonNode_PreparedState => true
            },
            peer
        ))
    }

    // tonNode.preparePersistentState block:tonNode.blockIdExt masterchain_block:tonNode.blockIdExt = tonNode.PreparedState;
    // tonNode.downloadPersistentState block:tonNode.blockIdExt masterchain_block:tonNode.blockIdExt = tonNode.Data; DEPRECATED?
    // tonNode.downloadPersistentStateSlice block:tonNode.blockIdExt masterchain_block:tonNode.blockIdExt offset:long max_size:long = tonNode.Data;
    async fn download_persistent_state_part(
        &self,
        block_id: &BlockIdExt,
        masterchain_block_id: &BlockIdExt,
        offset: usize,
        max_size: usize,
        peer: Arc<Neighbour>,
        attempt: u32,
    ) -> Result<Vec<u8>> {
        self.send_rldp_query_raw(
            &DownloadPersistentStateSlice {
                block: convert_block_id_ext_blk2api(block_id),
                masterchain_block: convert_block_id_ext_blk2api(masterchain_block_id),
                offset: offset as i64,
                max_size: max_size as i64,
            },
            peer,
            attempt
        ).await
    }

    // tonNode.prepareZeroState block:tonNode.blockIdExt = tonNode.PreparedState;
    // tonNode.downloadZeroState block:tonNode.blockIdExt = tonNode.Data;
    async fn download_zero_state(
        &self, 
        id: &BlockIdExt, 
    ) -> Result<Option<(ShardStateStuff, Vec<u8>)>> {
        // Prepare
        let (prepare, good_peer): (PreparedState, _) = self.send_adnl_query(
            TLObject::new(PrepareZeroState {
                block: convert_block_id_ext_blk2api(id),
            }),
            None,
            Some(Self::TIMEOUT_PREPARE)
        ).await?;

        // Download
        match prepare {
            PreparedState::TonNode_NotFoundState => Ok(None),
            PreparedState::TonNode_PreparedState => {
                let state_bytes = self.send_rldp_query_raw(
                    &DownloadZeroState {
                        block: convert_block_id_ext_blk2api(id),
                    },
                    good_peer,
                    0
                ).await?;
                Ok(
                    Some((
                        ShardStateStuff::deserialize_zerostate(id.clone(), &state_bytes)?,
                        state_bytes
                    ))
                )
            }
        }
    }

    // tonNode.keyBlocks blocks:(vector tonNode.blockIdExt) incomplete:Bool error:Bool = tonNode.KeyBlocks;
    // tonNode.getNextKeyBlockIds block:tonNode.blockIdExt max_size:int = tonNode.KeyBlocks;
    async fn download_next_key_blocks_ids(
        &self, 
        block_id: &BlockIdExt, 
        max_size: i32, 
    ) -> Result<Vec<BlockIdExt>> {
        let query = GetNextKeyBlockIds {
            block: convert_block_id_ext_blk2api(block_id),
            max_size
        };
        self.send_adnl_query(query, None, None)
            .await
            .and_then(|(ids, _): (KeyBlocks, _)| ids.blocks().iter().try_fold(Vec::new(), |mut vec, id| {
                vec.push(convert_block_id_ext_api2blk(id)?);
                Ok(vec)
            }))
    }
    
    // tonNode.downloadNextBlockFull prev_block:tonNode.blockIdExt = tonNode.DataFull;
    async fn download_next_block_full(
        &self, 
        prev_id: &BlockIdExt, 
    ) -> Result<Option<(BlockStuff, BlockProofStuff)>> {
        
        let request = &DownloadNextBlockFull {
            prev_block: convert_block_id_ext_blk2api(prev_id)
        };

        // Set neighbor
        let peer = self.peers.choose_neighbour()?.ok_or_else(||error!("neighbour is not found!"))?;
        log::trace!("USE PEER {}, REQUEST {:?}", peer.id(), request);
        
        // Download
        let data_full: DataFull = self.send_rldp_query_typed(
            request,
            peer,
            0
        ).await?;

        // Parse
        match data_full {
            DataFull::TonNode_DataFullEmpty => return Ok(None),
            DataFull::TonNode_DataFull(data_full) => {
                let id = convert_block_id_ext_api2blk(&data_full.id)?;
                let block = BlockStuff::deserialize_checked(id, data_full.block.to_vec())?;
                let proof = BlockProofStuff::deserialize(
                    block.id(), 
                    data_full.proof.to_vec(), 
                    data_full.is_link.clone().into()
                )?;
                Ok(Some((block, proof)))
            }
        }
    }

    async fn download_archive(&self, mc_seq_no: u32) -> Result<Option<Vec<u8>>> {
        const CHUNK_SIZE: i32 = 1 << 20;
        // tonNode.getArchiveInfo masterchain_seqno:int = tonNode.ArchiveInfo;
        let (archive_info, peer) = self.send_adnl_query(
            GetArchiveInfo {
                masterchain_seqno: mc_seq_no as i32
            },
            None,
            Some(Self::TIMEOUT_PREPARE)
        ).await?;

        match archive_info {
            ArchiveInfo::TonNode_ArchiveNotFound => Ok(None),
            ArchiveInfo::TonNode_ArchiveInfo(info) => {
                let mut result = Vec::new();
                let mut offset = 0;
                let mut part_attempt = 0;
                let mut peer_attempt = 0;
                loop {
                    let slice = GetArchiveSlice {
                        archive_id: info.id, offset, max_size: CHUNK_SIZE
                    };
                    match self.send_rldp_query_raw(&slice, peer.clone(), peer_attempt).await {
                        Ok(mut block_bytes) => {
                            let actual_size = block_bytes.len() as i32;
                            result.append(&mut block_bytes);
                            if actual_size < CHUNK_SIZE {
                                return Ok(Some(result));
                            }
                            offset += actual_size as i64;
                            part_attempt = 0;
                        },
                        Err(e) => {
                            peer_attempt += 1;
                            part_attempt += 1;
                            log::error!("download_archive {}: {}, offset: {}, attempt: {}",
                                info.id, e, offset, part_attempt);

                            if part_attempt > 10 {
                                fail!(
                                    "Error download_archive after {} attempts : {}", 
                                    part_attempt, e
                                )
                            }
                        }
                    };
                }
            },
        }
    }

    async fn wait_broadcast(&self) -> Result<Broadcast> {
        let receiver = self.overlay.clone();
        let id = self.overlay_id.clone();
        loop {
            match receiver.wait_for_broadcast(&id).await {
                Ok(data) => {
                    let answer: Broadcast = Deserializer::new(&mut Cursor::new(data.0)).read_boxed()?;
                    break Ok(answer)
                }
                Err(e) => log::error!("broadcast waiting error: {}", e)
            }
        }
    }

}
