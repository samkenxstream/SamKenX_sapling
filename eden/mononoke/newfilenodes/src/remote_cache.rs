/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use bytes::Bytes;
use caching_ext::MemcacheHandler;
use fbinit::FacebookInit;
use fbthrift::compact_protocol;
use futures_preview::{compat::Future01CompatExt, future::try_join_all};
use memcache::{KeyGen, MemcacheClient, MEMCACHE_VALUE_MAX_SIZE};
use mercurial_types::{HgFileNodeId, RepoPath};
use mononoke_types::RepositoryId;
use rand::random;
use stats::prelude::*;
use std::collections::HashSet;
use std::time::Duration;
use std::time::Instant;
use time_ext::DurationExt;
use tokio_preview;

use filenodes::{
    blake2_path_hash,
    thrift::{self, MC_CODEVER, MC_SITEVER},
    FilenodeInfo,
};

define_stats! {
    prefix = "mononoke.filenodes";
    gaf_compact_bytes: histogram(
        "get_all_filenodes.thrift_compact.bytes";
        500, 0, 1_000_000, Average, Sum, Count; P 50; P 95; P 99
    ),
    point_filenode_hit: timeseries("point_filenode.memcache.hit"; Sum),
    point_filenode_miss: timeseries("point_filenode.memcache.miss"; Sum),
    point_filenode_internal_err: timeseries("point_filenode.memcache.internal_err"; Sum),
    point_filenode_deserialize_err: timeseries("point_filenode.memcache.deserialize_err"; Sum),
    point_filenode_pointers_err: timeseries("point_filenode.memcache.pointers_err"; Sum),
    gaf_hit: timeseries("get_all_filenodes.memcache.hit"; Sum),
    gaf_miss: timeseries("get_all_filenodes.memcache.miss"; Sum),
    gaf_pointers: timeseries("get_all_filenodes.memcache.pointers"; Sum),
    gaf_internal_err: timeseries("get_all_filenodes.memcache.internal_err"; Sum),
    gaf_deserialize_err: timeseries("get_all_filenodes.memcache.deserialize_err"; Sum),
    gaf_pointers_err: timeseries("get_all_filenodes.memcache.pointers_err"; Sum),
    get_latency: histogram("get.memcache.duration_us"; 100, 0, 10000, Average, Count; P 50; P 95; P 100),
    get_history: histogram("get_history.memcache.duration_us"; 100, 0, 10000, Average, Count; P 50; P 95; P 100),
}

const SITEVER_OVERRIDE_VAR: &str = "MONONOKE_OVERRIDE_FILENODES_MC_SITEVER";

const TTL_SEC: u64 = 8 * 60 * 60;

// Adding a random to TTL helps preventing eviction of all related keys at once
const TTL_SEC_RAND: u64 = 30 * 60; // 30min

pub enum RemoteCache {
    Memcache(MemcacheCache),
    Noop,
}

impl RemoteCache {
    // TODO: Can we optimize to reuse the existing PathWithHash we got?
    pub async fn get_filenode(
        &self,
        repo_id: RepositoryId,
        path: &RepoPath,
        filenode_id: HgFileNodeId,
    ) -> Option<FilenodeInfo> {
        match self {
            Self::Memcache(memcache) => {
                let path_hash = PathHash::from_repo_path(path);

                let now = Instant::now();

                let ret = get_single_filenode_from_memcache(
                    &memcache.memcache,
                    &memcache.keygen,
                    repo_id,
                    filenode_id,
                    &path_hash,
                )
                .await;

                let elapsed = now.elapsed().as_micros_unchecked() as i64;
                STATS::get_latency.add_value(elapsed);

                ret
            }
            Self::Noop => None,
        }
    }

    pub fn fill_filenode(
        &self,
        repo_id: RepositoryId,
        path: &RepoPath,
        filenode_id: HgFileNodeId,
        filenode: FilenodeInfo,
    ) {
        match self {
            Self::Memcache(memcache) => {
                let path_hash = PathHash::from_repo_path(path);
                schedule_fill_filenode(
                    &memcache.memcache,
                    &memcache.keygen,
                    repo_id,
                    filenode_id,
                    &path_hash,
                    filenode,
                )
            }
            Self::Noop => {}
        }
    }

    pub async fn get_history(
        &self,
        repo_id: RepositoryId,
        path: &RepoPath,
    ) -> Option<Vec<FilenodeInfo>> {
        match self {
            Self::Memcache(memcache) => {
                let path_hash = PathHash::from_repo_path(path);

                let now = Instant::now();

                let ret = get_history_from_memcache(
                    &memcache.memcache,
                    &memcache.keygen,
                    repo_id,
                    &path_hash,
                )
                .await;

                let elapsed = now.elapsed().as_micros_unchecked() as i64;
                STATS::get_history.add_value(elapsed);

                ret
            }
            Self::Noop => None,
        }
    }

    pub fn fill_history(
        &self,
        repo_id: RepositoryId,
        path: &RepoPath,
        filenodes: Vec<FilenodeInfo>,
    ) {
        match self {
            Self::Memcache(memcache) => {
                let path_hash = PathHash::from_repo_path(path);
                schedule_fill_history(
                    memcache.memcache.clone(),
                    memcache.keygen.clone(),
                    repo_id,
                    path_hash,
                    filenodes,
                )
            }
            Self::Noop => {}
        }
    }
}

type Pointer = i64;

#[derive(Clone)]
struct PathHash(String);

pub struct MemcacheCache {
    memcache: MemcacheHandler,
    keygen: KeyGen,
}

impl PathHash {
    fn from_repo_path(repo_path: &RepoPath) -> Self {
        let path = match repo_path.mpath() {
            Some(repo_path) => repo_path.to_vec(),
            None => Vec::new(),
        };

        Self(blake2_path_hash(&path).to_string())
    }
}

impl MemcacheCache {
    pub fn new(fb: FacebookInit, backing_store_name: &str, backing_store_params: &str) -> Self {
        let key_prefix = format!(
            "scm.mononoke.filenodes.{}.{}",
            backing_store_name, backing_store_params,
        );

        let mc_sitever = match std::env::var(&SITEVER_OVERRIDE_VAR) {
            Ok(v) => v.parse().unwrap_or(MC_SITEVER as u32),
            Err(_) => MC_SITEVER as u32,
        };

        Self {
            memcache: MemcacheHandler::from(MemcacheClient::new(fb)),
            keygen: KeyGen::new(key_prefix, MC_CODEVER as u32, mc_sitever),
        }
    }
}

fn get_mc_key_for_single_filenode(
    keygen: &KeyGen,
    repo_id: RepositoryId,
    filenode: HgFileNodeId,
    path_hash: &PathHash,
) -> String {
    keygen.key(format!("{}.{}.{}", repo_id.id(), filenode, path_hash.0))
}

fn get_mc_key_for_filenodes_list(
    keygen: &KeyGen,
    repo_id: RepositoryId,
    path_hash: &PathHash,
) -> String {
    keygen.key(format!("{}.{}", repo_id.id(), path_hash.0))
}

fn get_mc_key_for_filenodes_list_chunk(
    keygen: &KeyGen,
    repo_id: RepositoryId,
    path_hash: &PathHash,
    pointer: Pointer,
) -> String {
    keygen.key(format!("{}.{}.{}", repo_id.id(), path_hash.0, pointer))
}

async fn get_single_filenode_from_memcache(
    memcache: &MemcacheHandler,
    keygen: &KeyGen,
    repo_id: RepositoryId,
    filenode: HgFileNodeId,
    path_hash: &PathHash,
) -> Option<FilenodeInfo> {
    let key = get_mc_key_for_single_filenode(&keygen, repo_id, filenode, path_hash);

    let serialized = match memcache.get(key).compat().await {
        Ok(Some(serialized)) => serialized,
        Ok(None) => {
            STATS::point_filenode_miss.add_value(1);
            return None;
        }
        Err(()) => {
            STATS::point_filenode_internal_err.add_value(1);
            return None;
        }
    };

    let thrift = match compact_protocol::deserialize(&Vec::from(serialized)) {
        Ok(thrift) => thrift,
        Err(_) => {
            STATS::point_filenode_deserialize_err.add_value(1);
            return None;
        }
    };

    let info = match FilenodeInfo::from_thrift(thrift) {
        Ok(info) => info,
        Err(_) => {
            STATS::point_filenode_deserialize_err.add_value(1);
            return None;
        }
    };

    STATS::point_filenode_hit.add_value(1);

    Some(info)
}

async fn get_history_from_memcache(
    memcache: &MemcacheHandler,
    keygen: &KeyGen,
    repo_id: RepositoryId,
    path_hash: &PathHash,
) -> Option<Vec<FilenodeInfo>> {
    // helper function for deserializing list of thrift FilenodeInfo into rust structure with proper
    // error returned
    fn deserialize_list(list: Vec<thrift::FilenodeInfo>) -> Option<Vec<FilenodeInfo>> {
        let res: Result<Vec<_>, _> = list.into_iter().map(FilenodeInfo::from_thrift).collect();
        if res.is_err() {
            STATS::gaf_deserialize_err.add_value(1);
        }
        res.ok()
    }

    let key = get_mc_key_for_filenodes_list(&keygen, repo_id, &path_hash);

    let serialized = match memcache.get(key).compat().await {
        Ok(Some(serialized)) => serialized,
        Ok(None) => {
            STATS::gaf_miss.add_value(1);
            return None;
        }
        Err(()) => {
            STATS::gaf_internal_err.add_value(1);
            return None;
        }
    };

    let thrift = match compact_protocol::deserialize(&Vec::from(serialized)) {
        Ok(thrift) => thrift,
        Err(_) => {
            STATS::gaf_deserialize_err.add_value(1);
            return None;
        }
    };

    let res = match thrift {
        thrift::FilenodeInfoList::UnknownField(_) => {
            STATS::gaf_deserialize_err.add_value(1);
            return None;
        }
        thrift::FilenodeInfoList::Data(list) => deserialize_list(list),
        thrift::FilenodeInfoList::Pointers(list) => {
            STATS::gaf_pointers.add_value(1);

            let read_chunks_fut = list.into_iter().map(move |pointer| {
                let key =
                    get_mc_key_for_filenodes_list_chunk(&keygen, repo_id, &path_hash, pointer);

                async move {
                    match memcache.get(key).compat().await {
                        Ok(Some(chunk)) => Ok(chunk),
                        _ => Err(()),
                    }
                }
            });

            let blob = match try_join_all(read_chunks_fut).await {
                Ok(chunks) => chunks.into_iter().flat_map(Vec::from).collect::<Vec<_>>(),
                Err(_) => {
                    STATS::gaf_pointers_err.add_value(1);
                    return None;
                }
            };

            match compact_protocol::deserialize(&blob) {
                Ok(thrift::FilenodeInfoList::Data(list)) => deserialize_list(list),
                _ => {
                    STATS::gaf_pointers_err.add_value(1);
                    None
                }
            }
        }
    };

    if res.is_some() {
        STATS::gaf_hit.add_value(1);
    }

    res
}

fn schedule_fill_filenode(
    memcache: &MemcacheHandler,
    keygen: &KeyGen,
    repo_id: RepositoryId,
    filenode_id: HgFileNodeId,
    path_hash: &PathHash,
    filenode: FilenodeInfo,
) {
    let serialized = compact_protocol::serialize(&filenode.into_thrift());

    // Quite unlikely that single filenode will be bigger than MEMCACHE_VALUE_MAX_SIZE
    // It's probably not even worth logging it
    if serialized.len() < MEMCACHE_VALUE_MAX_SIZE {
        let fut = memcache
            .set(
                get_mc_key_for_single_filenode(&keygen, repo_id, filenode_id, &path_hash),
                serialized,
            )
            .compat();

        tokio_preview::spawn(fut);
    }
}

fn schedule_fill_history(
    memcache: MemcacheHandler,
    keygen: KeyGen,
    repo_id: RepositoryId,
    path_hash: PathHash,
    filenodes: Vec<FilenodeInfo>,
) {
    let fut = async move {
        let _ = fill_history(&memcache, &keygen, repo_id, &path_hash, filenodes).await;
    };

    tokio_preview::spawn(fut);
}

fn serialize_history(filenodes: Vec<FilenodeInfo>) -> Bytes {
    let filenodes = thrift::FilenodeInfoList::Data(
        filenodes
            .into_iter()
            .map(|filenode_info| filenode_info.into_thrift())
            .collect(),
    );
    compact_protocol::serialize(&filenodes)
}

async fn fill_history(
    memcache: &MemcacheHandler,
    keygen: &KeyGen,
    repo_id: RepositoryId,
    path_hash: &PathHash,
    filenodes: Vec<FilenodeInfo>,
) -> Result<(), ()> {
    let serialized = serialize_history(filenodes);

    STATS::gaf_compact_bytes.add_value(serialized.len() as i64);

    let root = if serialized.len() < MEMCACHE_VALUE_MAX_SIZE {
        serialized
    } else {
        let write_chunks_fut = serialized
            .chunks(MEMCACHE_VALUE_MAX_SIZE)
            .map(Vec::from) // takes ownership
            .zip(PointersIter::new())
            .map({
                move |(chunk, pointer)| {
                    async move {
                        let chunk_key = get_mc_key_for_filenodes_list_chunk(
                            &keygen,
                            repo_id,
                            &path_hash,
                            pointer,
                        );

                        // give chunks non-random max TTL_SEC_RAND so that they always live
                        // longer than the pointer
                        let chunk_ttl = Duration::from_secs(TTL_SEC + TTL_SEC_RAND);

                        memcache.set_with_ttl(chunk_key, chunk, chunk_ttl).compat().await?;

                        Ok(pointer)
                    }
                }
            })
            .collect::<Vec<_>>();

        let pointers = try_join_all(write_chunks_fut).await?;
        compact_protocol::serialize(&thrift::FilenodeInfoList::Pointers(pointers))
    };

    let root_key = get_mc_key_for_filenodes_list(&keygen, repo_id, &path_hash);
    let root_ttl = Duration::from_secs(TTL_SEC + random::<u64>() % TTL_SEC_RAND);

    memcache
        .set_with_ttl(root_key, root, root_ttl)
        .compat()
        .await?;

    Ok(())
}

/// Infinite iterator over unique and random i64 values
struct PointersIter {
    seen: HashSet<Pointer>,
}

impl PointersIter {
    fn new() -> Self {
        Self {
            seen: HashSet::new(),
        }
    }
}

impl Iterator for PointersIter {
    type Item = Pointer;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let pointer = random();
            if self.seen.insert(pointer) {
                break Some(pointer);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use anyhow::Error;
    use mercurial_types_mocks::nodehash::{ONES_CSID, ONES_FNID};
    use mononoke_types::RepoPath;
    use mononoke_types_mocks::repo::{REPO_ONE, REPO_ZERO};
    use std::time::Duration;
    use tokio_preview as tokio;
    use tokio_preview::time;

    const TIMEOUT_MS: u64 = 100;
    const SLEEP_MS: u64 = 5;

    fn filenode() -> FilenodeInfo {
        FilenodeInfo {
            path: RepoPath::file("copiedto").unwrap(),
            filenode: ONES_FNID,
            p1: None,
            p2: None,
            copyfrom: Some((RepoPath::file("copiedfrom").unwrap(), ONES_FNID)),
            linknode: ONES_CSID,
        }
    }

    fn make_test_cache() -> RemoteCache {
        let keygen = KeyGen::new("newfilenodes.test", 0, 0);

        RemoteCache::Memcache(MemcacheCache {
            memcache: MemcacheHandler::create_mock(),
            keygen,
        })
    }

    #[fbinit::test]
    async fn test_store_filenode(_fb: FacebookInit) -> Result<(), Error> {
        let cache = make_test_cache();
        let info = filenode();

        cache.fill_filenode(REPO_ZERO, &info.path, info.filenode, info.clone());

        let from_cache = time::timeout(Duration::from_millis(TIMEOUT_MS), async {
            loop {
                match cache
                    .get_filenode(REPO_ZERO, &info.path, info.filenode)
                    .await
                {
                    Some(f) => {
                        break f;
                    }
                    None => {}
                }
                time::delay_for(Duration::from_millis(SLEEP_MS)).await;
            }
        })
        .await?;

        assert_eq!(from_cache, info);
        assert_eq!(
            None,
            cache
                .get_filenode(REPO_ONE, &info.path, info.filenode)
                .await
        );

        Ok(())
    }

    #[fbinit::test]
    async fn test_store_short_history(_fb: FacebookInit) -> Result<(), Error> {
        let cache = make_test_cache();
        let info = filenode();

        let history = vec![info.clone(), info.clone(), info.clone()];

        cache.fill_history(REPO_ZERO, &info.path, history.clone());

        let from_cache = time::timeout(Duration::from_millis(TIMEOUT_MS), async {
            loop {
                match cache.get_history(REPO_ZERO, &info.path).await {
                    Some(f) => {
                        break f;
                    }
                    None => {}
                }
                time::delay_for(Duration::from_millis(SLEEP_MS)).await;
            }
        })
        .await?;

        assert_eq!(from_cache, history);
        assert_eq!(None, cache.get_history(REPO_ONE, &info.path).await);

        Ok(())
    }

    #[fbinit::test]
    async fn test_store_long_history(_fb: FacebookInit) -> Result<(), Error> {
        let cache = make_test_cache();
        let info = filenode();

        let history = (0..100_000).map(|_| info.clone()).collect::<Vec<_>>();
        assert!(serialize_history(history.clone()).len() >= MEMCACHE_VALUE_MAX_SIZE);

        cache.fill_history(REPO_ZERO, &info.path, history.clone());

        let from_cache = time::timeout(Duration::from_millis(TIMEOUT_MS), async {
            loop {
                match cache.get_history(REPO_ZERO, &info.path).await {
                    Some(f) => {
                        break f;
                    }
                    None => {}
                }
                time::delay_for(Duration::from_millis(SLEEP_MS)).await;
            }
        })
        .await?;

        assert_eq!(from_cache, history);
        assert_eq!(None, cache.get_history(REPO_ONE, &info.path).await);

        Ok(())
    }
}
