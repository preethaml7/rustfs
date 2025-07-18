// Copyright 2024 RustFS Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use async_trait::async_trait;
use std::io::{Error, Result};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use crate::{Locker, lock_args::LockArgs};

pub const MAX_DELETE_LIST: usize = 1000;

#[derive(Clone, Debug)]
struct LockRequesterInfo {
    name: String,
    writer: bool,
    uid: String,
    time_stamp: Instant,
    time_last_refresh: Instant,
    source: String,
    group: bool,
    owner: String,
    quorum: usize,
    idx: usize,
}

impl Default for LockRequesterInfo {
    fn default() -> Self {
        Self {
            name: Default::default(),
            writer: Default::default(),
            uid: Default::default(),
            time_stamp: Instant::now(),
            time_last_refresh: Instant::now(),
            source: Default::default(),
            group: Default::default(),
            owner: Default::default(),
            quorum: Default::default(),
            idx: Default::default(),
        }
    }
}

fn is_write_lock(lri: &[LockRequesterInfo]) -> bool {
    lri.len() == 1 && lri[0].writer
}

#[derive(Debug, Default)]
pub struct LockStats {
    total: usize,
    writes: usize,
    reads: usize,
}

#[derive(Debug, Default)]
pub struct LocalLocker {
    lock_map: HashMap<String, Vec<LockRequesterInfo>>,
    lock_uid: HashMap<String, String>,
}

impl LocalLocker {
    pub fn new() -> Self {
        LocalLocker::default()
    }
}

impl LocalLocker {
    fn can_take_lock(&self, resource: &[String]) -> bool {
        resource.iter().fold(true, |acc, x| !self.lock_map.contains_key(x) && acc)
    }

    pub fn stats(&self) -> LockStats {
        let mut st = LockStats {
            total: self.lock_map.len(),
            ..Default::default()
        };

        self.lock_map.iter().for_each(|(_, value)| {
            if !value.is_empty() {
                if value[0].writer {
                    st.writes += 1;
                } else {
                    st.reads += 1;
                }
            }
        });

        st
    }

    fn dump_lock_map(&mut self) -> HashMap<String, Vec<LockRequesterInfo>> {
        let mut lock_copy = HashMap::new();
        self.lock_map.iter().for_each(|(key, value)| {
            lock_copy.insert(key.to_string(), value.to_vec());
        });

        lock_copy
    }

    fn expire_old_locks(&mut self, interval: Duration) {
        self.lock_map.iter_mut().for_each(|(_, lris)| {
            lris.retain(|lri| {
                if Instant::now().duration_since(lri.time_last_refresh) > interval {
                    let mut key = lri.uid.to_string();
                    format_uuid(&mut key, &lri.idx);
                    self.lock_uid.remove(&key);
                    return false;
                }

                true
            });
        });
    }
}

#[async_trait]
impl Locker for LocalLocker {
    async fn lock(&mut self, args: &LockArgs) -> Result<bool> {
        if args.resources.len() > MAX_DELETE_LIST {
            return Err(Error::other(format!(
                "internal error: LocalLocker.lock called with more than {MAX_DELETE_LIST} resources"
            )));
        }

        if !self.can_take_lock(&args.resources) {
            return Ok(false);
        }

        args.resources.iter().enumerate().for_each(|(idx, resource)| {
            self.lock_map.insert(
                resource.to_string(),
                vec![LockRequesterInfo {
                    name: resource.to_string(),
                    writer: true,
                    source: args.source.to_string(),
                    owner: args.owner.to_string(),
                    uid: args.uid.to_string(),
                    group: args.resources.len() > 1,
                    quorum: args.quorum,
                    idx,
                    ..Default::default()
                }],
            );

            let mut uuid = args.uid.to_string();
            format_uuid(&mut uuid, &idx);
            self.lock_uid.insert(uuid, resource.to_string());
        });

        Ok(true)
    }

    async fn unlock(&mut self, args: &LockArgs) -> Result<bool> {
        if args.resources.len() > MAX_DELETE_LIST {
            return Err(Error::other(format!(
                "internal error: LocalLocker.unlock called with more than {MAX_DELETE_LIST} resources"
            )));
        }

        let mut reply = false;
        let mut err_info = String::new();
        for resource in args.resources.iter() {
            match self.lock_map.get_mut(resource) {
                Some(lris) => {
                    if !is_write_lock(lris) {
                        if err_info.is_empty() {
                            err_info = format!("unlock attempted on a read locked entity: {resource}");
                        } else {
                            err_info.push_str(&format!(", {resource}"));
                        }
                    } else {
                        lris.retain(|lri| {
                            if lri.uid == args.uid && (args.owner.is_empty() || lri.owner == args.owner) {
                                let mut key = args.uid.to_string();
                                format_uuid(&mut key, &lri.idx);
                                self.lock_uid.remove(&key).unwrap();
                                reply |= true;
                                return false;
                            }

                            true
                        });
                    }
                    if lris.is_empty() {
                        self.lock_map.remove(resource);
                    }
                }
                None => {
                    continue;
                }
            };
        }

        Ok(reply)
    }

    async fn rlock(&mut self, args: &LockArgs) -> Result<bool> {
        if args.resources.len() != 1 {
            return Err(Error::other("internal error: localLocker.RLock called with more than one resource"));
        }

        let resource = &args.resources[0];
        match self.lock_map.get_mut(resource) {
            Some(lri) => {
                if !is_write_lock(lri) {
                    lri.push(LockRequesterInfo {
                        name: resource.to_string(),
                        writer: false,
                        source: args.source.to_string(),
                        owner: args.owner.to_string(),
                        uid: args.uid.to_string(),
                        quorum: args.quorum,
                        ..Default::default()
                    });
                } else {
                    return Ok(false);
                }
            }
            None => {
                self.lock_map.insert(
                    resource.to_string(),
                    vec![LockRequesterInfo {
                        name: resource.to_string(),
                        writer: false,
                        source: args.source.to_string(),
                        owner: args.owner.to_string(),
                        uid: args.uid.to_string(),
                        quorum: args.quorum,
                        ..Default::default()
                    }],
                );
            }
        }
        let mut uuid = args.uid.to_string();
        format_uuid(&mut uuid, &0);
        self.lock_uid.insert(uuid, resource.to_string());

        Ok(true)
    }

    async fn runlock(&mut self, args: &LockArgs) -> Result<bool> {
        if args.resources.len() != 1 {
            return Err(Error::other("internal error: localLocker.RLock called with more than one resource"));
        }

        let mut reply = false;
        let resource = &args.resources[0];
        match self.lock_map.get_mut(resource) {
            Some(lris) => {
                if is_write_lock(lris) {
                    return Err(Error::other(format!("runlock attempted on a write locked entity: {resource}")));
                } else {
                    lris.retain(|lri| {
                        if lri.uid == args.uid && (args.owner.is_empty() || lri.owner == args.owner) {
                            let mut key = args.uid.to_string();
                            format_uuid(&mut key, &lri.idx);
                            self.lock_uid.remove(&key).unwrap();
                            reply |= true;
                            return false;
                        }

                        true
                    });
                }
                if lris.is_empty() {
                    self.lock_map.remove(resource);
                }
            }
            None => {
                return Ok(reply);
            }
        };

        Ok(reply)
    }

    async fn refresh(&mut self, args: &LockArgs) -> Result<bool> {
        let mut idx = 0;
        let mut key = args.uid.to_string();
        format_uuid(&mut key, &idx);
        match self.lock_uid.get(&key) {
            Some(resource) => {
                let mut resource = resource;
                loop {
                    match self.lock_map.get_mut(resource) {
                        Some(_lris) => {}
                        None => {
                            let mut key = args.uid.to_string();
                            format_uuid(&mut key, &0);
                            self.lock_uid.remove(&key);
                            return Ok(idx > 0);
                        }
                    }

                    idx += 1;
                    let mut key = args.uid.to_string();
                    format_uuid(&mut key, &idx);
                    resource = match self.lock_uid.get(&key) {
                        Some(resource) => resource,
                        None => return Ok(true),
                    };
                }
            }
            None => Ok(false),
        }
    }

    // TODO: need add timeout mechanism
    async fn force_unlock(&mut self, args: &LockArgs) -> Result<bool> {
        if args.uid.is_empty() {
            args.resources.iter().for_each(|resource| {
                if let Some(lris) = self.lock_map.get(resource) {
                    lris.iter().for_each(|lri| {
                        let mut key = lri.uid.to_string();
                        format_uuid(&mut key, &lri.idx);
                        self.lock_uid.remove(&key);
                    });
                    if lris.is_empty() {
                        self.lock_map.remove(resource);
                    }
                }
            });

            return Ok(true);
        }
        let mut idx = 0;
        let mut need_remove_resource = Vec::new();
        let mut need_remove_map_id = Vec::new();
        let reply = loop {
            let mut map_id = args.uid.to_string();
            format_uuid(&mut map_id, &idx);
            match self.lock_uid.get(&map_id) {
                Some(resource) => match self.lock_map.get_mut(resource) {
                    Some(lris) => {
                        {
                            lris.retain(|lri| {
                                if lri.uid == args.uid && (args.owner.is_empty() || lri.owner == args.owner) {
                                    let mut key = args.uid.to_string();
                                    format_uuid(&mut key, &lri.idx);
                                    need_remove_map_id.push(key);
                                    return false;
                                }

                                true
                            });
                        }
                        idx += 1;
                        if lris.is_empty() {
                            need_remove_resource.push(resource.to_string());
                        }
                    }
                    None => {
                        need_remove_map_id.push(map_id);
                        idx += 1;
                        continue;
                    }
                },
                None => {
                    break idx > 0;
                }
            }
        };
        need_remove_resource.into_iter().for_each(|resource| {
            self.lock_map.remove(&resource);
        });
        need_remove_map_id.into_iter().for_each(|map_id| {
            self.lock_uid.remove(&map_id);
        });

        Ok(reply)
    }

    async fn close(&self) {}

    async fn is_online(&self) -> bool {
        true
    }

    async fn is_local(&self) -> bool {
        true
    }
}

fn format_uuid(s: &mut String, idx: &usize) {
    s.push_str(&idx.to_string());
}

#[cfg(test)]
mod test {
    use super::LocalLocker;
    use crate::{Locker, lock_args::LockArgs};
    use std::io::Result;
    use tokio;

    #[tokio::test]
    async fn test_lock_unlock() -> Result<()> {
        let mut local_locker = LocalLocker::new();
        let args = LockArgs {
            uid: "1111".to_string(),
            resources: vec!["dandan".to_string()],
            owner: "dd".to_string(),
            source: "".to_string(),
            quorum: 3,
        };
        local_locker.lock(&args).await?;

        println!("lock local_locker: {local_locker:?} \n");

        local_locker.unlock(&args).await?;
        println!("unlock local_locker: {local_locker:?}");

        Ok(())
    }
}
