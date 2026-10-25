//! The server daemon's Git service. One task owns the lazily started
//! `gitstatusd` client, so queries stay serial on its single pipe, and keeps
//! per-query state: the last successful result, the in-flight query with its
//! start time, and the waiters for one shared follow-up query.
//!
//! Freshness: a request that arrives at time `t` is answered only by a query
//! that started at or after `t`, so a change made just before a prompt is
//! never hidden by an older in-flight query. Every request arriving while an
//! older query for its key runs joins that key's single follow-up, which is
//! what keeps concurrent or rapid prompts from queueing one query each.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::mem;
use std::pin::Pin;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};

use crate::gitstatus::{self, Query, Snapshot};

const GITSTATUS_TIMEOUT: Duration = Duration::from_secs(30);
/// Keys retained after their queries finish, evicting the least recently
/// requested. Keys with in-flight or queued work are never evicted.
const CACHE_LIMIT: usize = 64;

/// A query outcome shared by every waiter it satisfies. Errors travel as the
/// text the wire protocol transmits.
pub(super) type GitResult = Result<Option<Snapshot>, String>;

/// What one status request receives: the cached result for its key, if any,
/// immediately, and the result of the covering query once it completes.
/// Dropping `fresh` withdraws the request without affecting a started query.
pub(super) struct Admission {
    pub(super) stale: Option<Cached>,
    pub(super) fresh: oneshot::Receiver<GitResult>,
}

/// A key's last successful result: a snapshot, or `None` for a directory
/// outside any repository. Errors are never cached.
#[derive(Clone)]
pub(super) struct Cached(pub(super) Option<Snapshot>);

#[derive(Clone)]
pub(super) struct GitService {
    commands: mpsc::UnboundedSender<Command>,
}

enum Command {
    Status {
        query: Query,
        arrived: Instant,
        reply: oneshot::Sender<Admission>,
    },
    Reset {
        reply: oneshot::Sender<io::Result<()>>,
    },
}

impl GitService {
    /// Starts the owner task. Commands are bounded by the daemon's connection
    /// admission (one per connection), so the channel needs no own bound.
    pub(super) fn spawn() -> (Self, JoinHandle<()>) {
        let (commands, receiver) = mpsc::unbounded_channel();
        let owner = tokio::spawn(Owner::default().run(receiver));
        (Self { commands }, owner)
    }

    /// Registers a request that arrived at `arrived`.
    pub(super) async fn status(&self, query: Query, arrived: Instant) -> io::Result<Admission> {
        let (reply, admission) = oneshot::channel();
        self.send(Command::Status {
            query,
            arrived,
            reply,
        })?;
        admission.await.map_err(|_| stopped())
    }

    /// Clears every cached result and restarts an already started
    /// `gitstatusd`; it never starts an unused one.
    pub(super) async fn reset(&self) -> io::Result<()> {
        let (reply, result) = oneshot::channel();
        self.send(Command::Reset { reply })?;
        result.await.map_err(|_| stopped())?
    }

    fn send(&self, command: Command) -> io::Result<()> {
        self.commands.send(command).map_err(|_| stopped())
    }
}

fn stopped() -> io::Error {
    io::Error::other("Git service stopped")
}

type Running = Pin<Box<dyn Future<Output = Completion> + Send>>;

struct Completion {
    query: Query,
    epoch: u64,
    client: Option<gitstatus::Client>,
    result: GitResult,
}

#[derive(Default)]
struct Owner {
    /// `None` before the first query and while lent to the running query.
    client: Option<gitstatus::Client>,
    keys: HashMap<Query, Key>,
    /// Keys with a pending follow-up, in arrival order.
    queue: VecDeque<Query>,
    running: Option<Running>,
    /// Incremented by RESET so a query started before it is not cached.
    epoch: u64,
    restart_after_query: bool,
    tick: u64,
}

#[derive(Default)]
struct Key {
    cached: Option<Cached>,
    /// Start time of this key's in-flight query.
    started: Option<Instant>,
    /// Waiters the in-flight query satisfies.
    joined: Vec<oneshot::Sender<GitResult>>,
    /// Waiters for the follow-up query.
    pending: Vec<oneshot::Sender<GitResult>>,
    queued: bool,
    used: u64,
}

impl Key {
    fn idle(&self) -> bool {
        self.started.is_none() && !self.queued
    }
}

enum Event {
    Command(Command),
    Completed(Box<Completion>),
    Closed,
}

impl Owner {
    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        loop {
            let event = tokio::select! {
                command = commands.recv() => command.map_or(Event::Closed, Event::Command),
                completion = async { self.running.as_mut().expect("guarded").await },
                    if self.running.is_some() => Event::Completed(Box::new(completion)),
            };
            match event {
                Event::Command(Command::Status {
                    query,
                    arrived,
                    reply,
                }) => {
                    let _ = reply.send(self.admit(query, arrived));
                }
                Event::Command(Command::Reset { reply }) => {
                    let _ = reply.send(self.reset());
                }
                Event::Completed(completion) => self.complete(*completion),
                Event::Closed => return,
            }
            self.evict();
            self.start_next();
        }
    }

    fn admit(&mut self, query: Query, arrived: Instant) -> Admission {
        self.tick += 1;
        let key = self.keys.entry(query.clone()).or_default();
        key.used = self.tick;
        let (waiter, fresh) = oneshot::channel();
        if key.started.is_some_and(|started| started >= arrived) {
            key.joined.push(waiter);
        } else {
            key.pending.push(waiter);
            if !mem::replace(&mut key.queued, true) {
                self.queue.push_back(query);
            }
        }
        Admission {
            stale: key.cached.clone(),
            fresh,
        }
    }

    fn reset(&mut self) -> io::Result<()> {
        self.epoch += 1;
        self.keys.retain(|_, key| !key.idle());
        for key in self.keys.values_mut() {
            key.cached = None;
        }
        if self.running.is_some() {
            // The client is lent to the running query; replace it on return.
            self.restart_after_query = true;
            return Ok(());
        }
        self.client
            .as_mut()
            .map_or(Ok(()), gitstatus::Client::restart)
    }

    fn complete(&mut self, completion: Completion) {
        self.running = None;
        self.client = completion.client;
        if mem::take(&mut self.restart_after_query)
            && let Some(client) = self.client.as_mut()
            && let Err(error) = client.restart()
        {
            eprintln!("ztheme: gitstatusd restart after reset failed: {error}");
        }
        let Some(key) = self.keys.get_mut(&completion.query) else {
            return;
        };
        key.started = None;
        for waiter in key.joined.drain(..) {
            let _ = waiter.send(completion.result.clone());
        }
        if completion.epoch == self.epoch
            && let Ok(value) = completion.result
        {
            key.cached = Some(Cached(value));
        }
    }

    fn evict(&mut self) {
        while self.keys.len() > CACHE_LIMIT {
            let Some(victim) = self
                .keys
                .iter()
                .filter(|(_, key)| key.idle())
                .min_by_key(|(_, key)| key.used)
                .map(|(query, _)| query.clone())
            else {
                return;
            };
            self.keys.remove(&victim);
        }
    }

    /// Starts the oldest follow-up that still has a waiter. A follow-up whose
    /// requesters all disconnected is dropped before it reaches gitstatusd.
    fn start_next(&mut self) {
        while self.running.is_none()
            && let Some(query) = self.queue.pop_front()
        {
            let Some(key) = self.keys.get_mut(&query) else {
                continue;
            };
            key.queued = false;
            key.pending.retain(|waiter| !waiter.is_closed());
            if key.pending.is_empty() {
                continue;
            }
            key.joined = mem::take(&mut key.pending);
            key.started = Some(Instant::now());
            self.running = Some(Box::pin(execute(self.client.take(), query, self.epoch)));
        }
    }
}

/// Runs one query to completion, starting `gitstatusd` on first use. It is
/// never cancelled by a requester: abandoning a reply would desynchronize
/// the shared pipe, and the result still refreshes the cache.
async fn execute(client: Option<gitstatus::Client>, query: Query, epoch: u64) -> Completion {
    let (client, result) = match client.map_or_else(gitstatus::Client::start, Ok) {
        Ok(mut client) => {
            let result = run_query(&mut client, &query).await;
            (Some(client), result)
        }
        Err(error) => (None, Err(error)),
    };
    Completion {
        query,
        epoch,
        client,
        result: result.map_err(|error| error.to_string()),
    }
}

async fn run_query(client: &mut gitstatus::Client, query: &Query) -> io::Result<Option<Snapshot>> {
    if let Ok(result) = timeout(GITSTATUS_TIMEOUT, client.query(query)).await {
        return result;
    }
    match client.restart() {
        Ok(()) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "gitstatusd query exceeded 30 seconds",
        )),
        Err(error) => Err(io::Error::other(format!(
            "gitstatusd query timed out and restart failed: {error}"
        ))),
    }
}
