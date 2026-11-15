//! Ref notices: who listens to which workspace, and how a listener is told to stop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use tokio::sync::{broadcast, oneshot};

use crate::git::push::RefMove;

/// Moves a stream may fall behind by before it is ended.
pub const CAPACITY: usize = 256;

/// The in-process fan-out: a broadcast topic per workspace and a registry of open streams keyed
/// by workspace, user and token, so a revocation, a disabling, a lost grant or a shutdown closes
/// exactly the streams it should. Clones share one registry.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Notices {
    inner: Arc<Mutex<Registry>>,
}

#[derive(Debug, Default)]
struct Registry {
    topics: HashMap<i64, broadcast::Sender<RefMove>>,
    streams: Vec<Entry>,
    next_id: u64,
    closed: bool,
}

// A close is the drop of an entry: its `_close` sender goes with it and the stream's
// oneshot receiver wakes with an error.
#[derive(Debug)]
struct Entry {
    id: u64,
    workspace_id: i64,
    user_id: i64,
    token_id: i64,
    _close: oneshot::Sender<()>,
}

/// One open stream's view of its workspace. Dropping it unregisters the stream.
#[derive(Debug)]
#[non_exhaustive]
pub struct Subscription {
    // Declared first so it is dropped before the guard checks whether the topic still has
    // receivers.
    moves: broadcast::Receiver<RefMove>,
    closed: oneshot::Receiver<()>,
    // Set once `next` has returned `None`: a finished oneshot panics if polled again, and a
    // lagged receiver would otherwise resume.
    ended: bool,
    _guard: Guard,
}

#[derive(Debug)]
struct Guard {
    id: u64,
    workspace_id: i64,
    // Weak, so a stream outliving every `Notices` clone does not keep the registry alive.
    inner: Weak<Mutex<Registry>>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut reg = lock(&inner);
        reg.streams.retain(|e| e.id != self.id);
        // Drop the topic once its last receiver is gone, so the map stays bounded by open streams.
        if reg
            .topics
            .get(&self.workspace_id)
            .is_some_and(|t| t.receiver_count() == 0)
        {
            reg.topics.remove(&self.workspace_id);
        }
    }
}

impl Subscription {
    /// The next move; `None` once the server closed this stream, once it lagged by more than
    /// `CAPACITY` moves, or once the registry is gone.
    pub async fn next(&mut self) -> Option<RefMove> {
        if self.ended {
            return None;
        }
        let next = tokio::select! {
            _ = &mut self.closed => None,
            // A lag ends the stream too: the client's reconnect snapshot resynchronises it.
            r = self.moves.recv() => r.ok(),
        };
        self.ended = next.is_none();
        next
    }
}

fn lock(inner: &Mutex<Registry>) -> MutexGuard<'_, Registry> {
    inner.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Notices {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens a stream on `workspace_id` for `user_id`, authenticated by `token_id`.
    pub fn subscribe(&self, workspace_id: i64, user_id: i64, token_id: i64) -> Subscription {
        let mut reg = lock(&self.inner);
        let id = reg.next_id;
        reg.next_id += 1;
        let moves = reg
            .topics
            .entry(workspace_id)
            .or_insert_with(|| broadcast::channel(CAPACITY).0)
            .subscribe();
        let (close, closed) = oneshot::channel();
        // After shutdown the sender is dropped on the spot: the stream ends at once and is
        // never registered.
        if !reg.closed {
            reg.streams.push(Entry {
                id,
                workspace_id,
                user_id,
                token_id,
                _close: close,
            });
        }
        Subscription {
            moves,
            closed,
            ended: false,
            _guard: Guard {
                id,
                workspace_id,
                inner: Arc::downgrade(&self.inner),
            },
        }
    }

    /// Delivers `moves`, in order, to every stream on `workspace_id`.
    pub fn publish(&self, workspace_id: i64, moves: &[RefMove]) {
        let reg = lock(&self.inner);
        if let Some(topic) = reg.topics.get(&workspace_id) {
            for m in moves {
                // A topic whose receivers are all gone has nobody to tell.
                let _ = topic.send(m.clone());
            }
        }
    }

    /// Closes the streams opened with `token_id`; returns how many.
    pub fn close_token(&self, token_id: i64) -> usize {
        self.close(|e| e.token_id == token_id)
    }

    /// Closes every stream of `user_id`.
    pub fn close_user(&self, user_id: i64) -> usize {
        self.close(|e| e.user_id == user_id)
    }

    /// Closes `user_id`'s streams on `workspace_id`.
    pub fn close_grant(&self, workspace_id: i64, user_id: i64) -> usize {
        self.close(|e| e.workspace_id == workspace_id && e.user_id == user_id)
    }

    /// Closes every stream on `workspace_id`.
    pub fn close_workspace(&self, workspace_id: i64) -> usize {
        self.close(|e| e.workspace_id == workspace_id)
    }

    /// Closes every stream and marks the registry closed: every later `subscribe` returns a
    /// stream that ends at once and is never registered. For shutdown, where the listeners are
    /// still accepting until the stop flag is sent.
    pub fn close_all(&self) -> usize {
        let mut reg = lock(&self.inner);
        reg.closed = true;
        let open = reg.streams.len();
        reg.streams.clear();
        open
    }

    /// The workspaces `user_id` has streams on, deduplicated, ascending.
    pub fn workspaces_of(&self, user_id: i64) -> Vec<i64> {
        let mut ids: Vec<i64> = lock(&self.inner)
            .streams
            .iter()
            .filter(|e| e.user_id == user_id)
            .map(|e| e.workspace_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// How many streams are open.
    pub fn open(&self) -> usize {
        lock(&self.inner).streams.len()
    }

    // Drops the matching entries, firing their close handles; returns how many went.
    fn close(&self, matches: impl Fn(&Entry) -> bool) -> usize {
        let mut reg = lock(&self.inner);
        let before = reg.streams.len();
        reg.streams.retain(|e| !matches(e));
        before - reg.streams.len()
    }
}
