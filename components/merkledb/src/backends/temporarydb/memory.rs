// Copyright 2022 The Exonum Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! An implementation of `TemporaryDB` database stored in RAM.

use smallvec::SmallVec;
use std::{
    collections::{btree_map::Range, BTreeMap},
    iter::{Iterator, Peekable},
    sync::{Arc, RwLock},
};

use crate::{
    backends::rocksdb::{next_id_bytes, ID_SIZE},
    db::{check_database, Change, Iterator as DbIterator},
    Database, Iter, Patch, ResolvedAddress, Result, Snapshot,
};

type MemoryDB = im::HashMap<ResolvedAddress, BTreeMap<Vec<u8>, Vec<u8>>>;

/// This in-memory database is only used for testing and experimenting; is not designed to
/// operate under load in production.
#[derive(Debug)]
pub struct TemporaryDB {
    inner: Arc<RwLock<MemoryDB>>,
}

struct TemporarySnapshot {
    snapshot: MemoryDB,
}

struct TemporaryDBIterator<'a> {
    iter: Peekable<Range<'a, Vec<u8>, Vec<u8>>>,
    prefix: Option<[u8; ID_SIZE]>,
    ended: bool,
}

impl TemporaryDB {
    /// Creates a new, empty database.
    pub fn new() -> Self {
        let mut db = im::HashMap::new();

        db.insert(ResolvedAddress::system("default"), BTreeMap::new());
        let inner = Arc::new(RwLock::new(db));
        let mut db = Self { inner };
        check_database(&mut db).unwrap();
        db
    }

    /// Clears the contents of the database.
    pub fn clear(&self) -> Result<()> {
        let mut rw_lock = self.inner.write().expect("Couldn't get read-write lock");
        let empty_tables = rw_lock
            .keys()
            .map(|k| (k.clone(), BTreeMap::default()))
            .collect::<Vec<_>>();

        rw_lock.clear();
        rw_lock.extend(empty_tables);

        Ok(())
    }

    fn temporary_snapshot(&self) -> TemporarySnapshot {
        TemporarySnapshot {
            snapshot: self.inner.read().expect("Couldn't get read lock").clone(),
        }
    }
}

impl Database for TemporaryDB {
    fn snapshot(&self) -> Box<dyn Snapshot> {
        Box::new(self.temporary_snapshot())
    }

    fn merge(&self, patch: Patch) -> Result<()> {
        let mut inner = self.inner.write().expect("Couldn't get write lock");
        for (resolved, changes) in patch.into_changes() {
            if !inner.contains_key(&resolved) {
                inner.insert(resolved.clone(), BTreeMap::new());
            }

            let collection: &mut BTreeMap<Vec<u8>, Vec<u8>> = inner.get_mut(&resolved).unwrap();

            if changes.is_cleared() {
                if let Some(id_bytes) = resolved.id_to_bytes() {
                    let next_bytes = next_id_bytes(id_bytes);
                    let mut middle_and_tail = collection.split_off(id_bytes.as_ref());
                    let mut tail = middle_and_tail.split_off(next_bytes.as_ref());
                    collection.append(&mut tail);
                } else {
                    collection.clear();
                }
            }

            if let Some(id_bytes) = resolved.id_to_bytes() {
                // Write changes to the column family with each key prefixed by the ID of the
                // resolved address.

                // We assume that typical key sizes are less than `1_024 - ID_SIZE = 1_016` bytes,
                // so that they fit into stack.
                let mut buffer: SmallVec<[u8; 1_024]> = SmallVec::new();
                buffer.extend_from_slice(&id_bytes);

                for (key, change) in changes.into_data() {
                    buffer.truncate(ID_SIZE);
                    buffer.extend_from_slice(&key);

                    match change {
                        Change::Put(value) => collection.insert(buffer.to_vec(), value),
                        Change::Delete => collection.remove(buffer.as_ref()),
                    };
                }
            } else {
                // Write changes to the column family as-is.
                for (key, change) in changes.into_data() {
                    match change {
                        Change::Put(value) => collection.insert(key, value),
                        Change::Delete => collection.remove(&key),
                    };
                }
            }
        }
        Ok(())
    }

    fn merge_sync(&self, patch: Patch) -> Result<()> {
        self.merge(patch)
    }
}

impl<'a> DbIterator for TemporaryDBIterator<'a> {
    fn next(&mut self) -> Option<(&[u8], &[u8])> {
        if self.ended {
            return None;
        }

        let (key, value) = self.iter.next()?;

        if let Some(ref prefix) = self.prefix {
            if &key[..ID_SIZE] != prefix {
                self.ended = true;
                return None;
            }
        }

        let key = if self.prefix.is_some() {
            &key[ID_SIZE..]
        } else {
            &key[..]
        };

        Some((key, value))
    }

    fn peek(&mut self) -> Option<(&[u8], &[u8])> {
        if self.ended {
            return None;
        }

        let (key, value) = self.iter.peek()?;
        let key = if let Some(prefix) = self.prefix {
            if key[..ID_SIZE] != prefix {
                self.ended = true;
                return None;
            }
            &key[ID_SIZE..]
        } else {
            &key[..]
        };

        Some((key, value))
    }
}

impl Snapshot for TemporarySnapshot {
    fn get(&self, name: &ResolvedAddress, key: &[u8]) -> Option<Vec<u8>> {
        let collection = self.snapshot.get(name)?;
        collection.get(name.keyed(key).as_ref()).cloned()
    }

    fn iter(&self, name: &ResolvedAddress, from: &[u8]) -> Iter<'_> {
        let collection = self
            .snapshot
            .get(name)
            .or_else(|| self.snapshot.get(&ResolvedAddress::system("default")))
            .unwrap();
        let from = name.keyed(from).into_owned();
        let iter = collection.range::<Vec<u8>, _>(&from..);

        Box::new(TemporaryDBIterator {
            iter: iter.peekable(),
            prefix: name.id_to_bytes(),
            ended: false,
        })
    }
}
