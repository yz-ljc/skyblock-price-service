use std::{
    collections::{BTreeMap, hash_map::RandomState},
    hash::BuildHasher,
    time::{Duration, Instant},
};

pub const TTL: Duration = Duration::from_secs(15 * 60);
pub const MAX_SESSIONS: usize = 128;
pub const MAX_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_SESSION_BYTES: usize = 8 * 1024 * 1024;

pub struct Session<M> {
    pub context: String,
    pub rows: Box<[Box<str>]>,
    pub metadata: M,
    pub created: Instant,
    pub bytes: usize,
}

/// Compact response rows only: no retained market snapshots or raw auction data.
/// The caller holds the cache lock while building a session, bounding temporary storage too.
pub struct SearchCache<M> {
    entries: BTreeMap<String, Session<M>>,
    random: RandomState,
    sequence: u64,
}

impl<M> Default for SearchCache<M> {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            random: RandomState::new(),
            sequence: 0,
        }
    }
}

impl<M> SearchCache<M> {
    pub fn prune(&mut self) {
        self.entries
            .retain(|_, session| session.created.elapsed() < TTL);
    }

    pub fn get(&self, id: &str) -> Option<&Session<M>> {
        self.entries.get(id)
    }

    /// Evict the oldest searches under pressure. IDs use a process-random seed so
    /// restarting the service cannot silently associate old cursors with new searches.
    pub fn insert(&mut self, session: Session<M>) -> String {
        while self.entries.len() >= MAX_SESSIONS
            || self.entries.values().map(|v| v.bytes).sum::<usize>() + session.bytes > MAX_BYTES
        {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, v)| v.created)
                .map(|(id, _)| id.clone());
            if let Some(id) = oldest {
                self.entries.remove(&id);
            } else {
                break;
            }
        }
        let id = loop {
            self.sequence = self.sequence.wrapping_add(1);
            let id = format!("{:016x}", self.random.hash_one(self.sequence));
            if !self.entries.contains_key(&id) {
                break id;
            }
        };
        self.entries.insert(id.clone(), session);
        id
    }
}
