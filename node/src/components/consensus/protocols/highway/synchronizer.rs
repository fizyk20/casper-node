use std::{
    collections::{BTreeMap, HashSet},
    fmt::Debug,
};

use datasize::DataSize;
use itertools::Itertools;

use crate::{
    components::consensus::{
        consensus_protocol::ProtocolOutcome,
        highway_core::highway::{Dependency, Highway, PreValidatedVertex, Vertex},
        traits::{Context, NodeIdT},
    },
    types::{TimeDiff, Timestamp},
};

use super::{ProtocolOutcomes, ACTION_ID_VERTEX};

/// An incoming pre-validated vertex that we haven't added to the protocol state yet.
#[derive(DataSize, Debug)]
pub(crate) struct PendingVertex<I, C>
where
    C: Context,
{
    /// The peer who sent it to us.
    sender: I,
    /// The pre-validated vertex.
    pvv: PreValidatedVertex<C>,
    /// The time when we received it.
    time_received: Timestamp,
}

impl<I, C: Context> PendingVertex<I, C> {
    /// Returns a new pending vertex with the current timestamp.
    pub(crate) fn new(sender: I, pvv: PreValidatedVertex<C>, time_received: Timestamp) -> Self {
        Self {
            sender,
            pvv,
            time_received,
        }
    }

    /// Returns the peer from which we received this vertex.
    pub(crate) fn sender(&self) -> &I {
        &self.sender
    }

    /// Returns the vertex waiting to be added.
    pub(crate) fn vertex(&self) -> &Vertex<C> {
        self.pvv.inner()
    }

    /// Returns the pre-validated vertex.
    pub(crate) fn pvv(&self) -> &PreValidatedVertex<C> {
        &self.pvv
    }

    /// Returns `true` if this vertex was received earlier than at the `oldest` timestamp.
    fn expired(&self, oldest: Timestamp) -> bool {
        self.time_received < oldest
    }
}

impl<I, C: Context> Into<PreValidatedVertex<C>> for PendingVertex<I, C> {
    fn into(self) -> PreValidatedVertex<C> {
        self.pvv
    }
}

#[derive(DataSize, Debug)]
pub(crate) struct Synchronizer<I, C>
where
    C: Context,
{
    /// Incoming vertices we can't add yet because they are still missing a dependency.
    vertex_deps: BTreeMap<Dependency<C>, Vec<PendingVertex<I, C>>>,
    /// The vertices that are scheduled to be processed at a later time.  The keys of this
    /// `BTreeMap` are timestamps when the corresponding vector of vertices will be added.
    vertices_to_be_added_later: BTreeMap<Timestamp, Vec<PendingVertex<I, C>>>,
    /// Vertices that might be ready to add to the protocol state: We are not currently waiting for
    /// a requested dependency.
    vertices_to_be_added: Vec<PendingVertex<I, C>>,
    /// The duration for which incoming vertices with missing dependencies are kept in a queue.
    pending_vertex_timeout: TimeDiff,
}

impl<I: NodeIdT, C: Context + 'static> Synchronizer<I, C> {
    /// Creates a new synchronizer with the specified timeout for pending vertices.
    pub(crate) fn new(pending_vertex_timeout: TimeDiff) -> Self {
        Synchronizer {
            vertex_deps: BTreeMap::new(),
            vertices_to_be_added_later: BTreeMap::new(),
            vertices_to_be_added: Vec::new(),
            pending_vertex_timeout,
        }
    }

    /// Removes expired pending vertices from the queues, and schedules the next purge.
    pub(crate) fn purge_vertices(&mut self, now: Timestamp) {
        let oldest = now.saturating_sub(self.pending_vertex_timeout);
        self.vertices_to_be_added.retain(|pv| !pv.expired(oldest));
        Self::remove_expired(&mut self.vertices_to_be_added_later, oldest);
        Self::remove_expired(&mut self.vertex_deps, oldest);
    }

    /// Store a (pre-validated) vertex which will be added later.  This creates a timer to be sent
    /// to the reactor. The vertex be added using `Self::add_vertices` when that timer goes off.
    pub(crate) fn store_vertex_for_addition_later(
        &mut self,
        future_timestamp: Timestamp,
        now: Timestamp,
        sender: I,
        pvv: PreValidatedVertex<C>,
    ) {
        self.vertices_to_be_added_later
            .entry(future_timestamp)
            .or_default()
            .push(PendingVertex::new(sender, pvv, now));
    }

    /// Schedules calls to `add_vertex` on any vertices in `vertices_to_be_added_later` which are
    /// scheduled for after the given `transpired_timestamp`.  In general the specified `timestamp`
    /// is approximately `Timestamp::now()`.  Vertices keyed by timestamps chronologically before
    /// `transpired_timestamp` should all be added.
    pub(crate) fn add_past_due_stored_vertices(
        &mut self,
        timestamp: Timestamp,
    ) -> ProtocolOutcomes<I, C> {
        let mut results = vec![];
        let past_due_timestamps: Vec<Timestamp> = self
            .vertices_to_be_added_later
            .range(..=timestamp) // Inclusive range
            .map(|(past_due_timestamp, _)| past_due_timestamp.to_owned())
            .collect();
        for past_due_timestamp in past_due_timestamps {
            if let Some(vertices_to_add) =
                self.vertices_to_be_added_later.remove(&past_due_timestamp)
            {
                results.extend(self.schedule_add_vertices(vertices_to_add))
            }
        }
        results
    }

    /// Schedules vertices to be added to the protocol state.
    pub(crate) fn schedule_add_vertices<T>(&mut self, pending_vertices: T) -> ProtocolOutcomes<I, C>
    where
        T: IntoIterator<Item = PendingVertex<I, C>>,
    {
        let was_empty = self.vertices_to_be_added.is_empty();
        self.vertices_to_be_added.extend(pending_vertices);
        if was_empty && !self.vertices_to_be_added.is_empty() {
            vec![ProtocolOutcome::QueueAction(ACTION_ID_VERTEX)]
        } else {
            Vec::new()
        }
    }

    /// Moves all vertices whose known missing dependency is now satisfied into the
    /// `vertices_to_be_added` queue.
    pub(crate) fn remove_satisfied_deps(&mut self, highway: &Highway<C>) -> ProtocolOutcomes<I, C> {
        let satisfied_deps = self
            .vertex_deps
            .keys()
            .filter(|dep| highway.has_dependency(dep))
            .cloned()
            .collect_vec();
        let pvs = satisfied_deps
            .into_iter()
            .flat_map(|dep| self.vertex_deps.remove(&dep).unwrap())
            .collect_vec();
        self.schedule_add_vertices(pvs)
    }

    /// Pops and returns the next entry from `vertices_to_be_added` that is not yet in the protocol
    /// state. Also returns a `ProtocolOutcome` that schedules the next action to add a vertex,
    /// unless the queue is empty.
    pub(crate) fn pop_vertex_to_add(
        &mut self,
        highway: &Highway<C>,
    ) -> Option<(PendingVertex<I, C>, ProtocolOutcomes<I, C>)> {
        // Get the next vertex to be added; skip the ones that are already in the protocol state.
        let pv = loop {
            let pv = self.vertices_to_be_added.pop()?;
            if highway.has_vertex(pv.vertex()) {
                continue; // This vertex was already added. Try the next one.
            }
            break pv;
        };
        if self.vertices_to_be_added.is_empty() {
            // Found next vertex, but the queue is empty: No need to schedule another call.
            Some((pv, Vec::new()))
        } else {
            // There are still vertices in the queue: schedule next call.
            Some((pv, vec![ProtocolOutcome::QueueAction(ACTION_ID_VERTEX)]))
        }
    }

    /// Adds a vertex with a known missing dependency to the queue.
    pub(crate) fn add_missing_dependency(&mut self, dep: Dependency<C>, pv: PendingVertex<I, C>) {
        self.vertex_deps.entry(dep).or_default().push(pv)
    }

    /// Returns `true` if no vertices are in the queues.
    pub(crate) fn is_empty(&self) -> bool {
        self.vertex_deps.is_empty()
            && self.vertices_to_be_added.is_empty()
            && self.vertices_to_be_added_later.is_empty()
    }

    /// Returns `true` if there are any vertices waiting for the specified dependency.
    pub(crate) fn is_dependency(&self, dep: &Dependency<C>) -> bool {
        self.vertex_deps.contains_key(dep)
    }

    /// Returns the timeout for pending vertices: Entries older than this are purged periodically.
    pub(crate) fn pending_vertex_timeout(&self) -> TimeDiff {
        self.pending_vertex_timeout
    }

    /// Drops all vertices that (directly or indirectly) have the specified dependencies, and
    /// returns the set of their senders. If the specified dependencies are known to be invalid,
    /// those senders must be faulty.
    pub(crate) fn drop_dependent_vertices(
        &mut self,
        mut vertices: Vec<Dependency<C>>,
    ) -> HashSet<I> {
        let mut senders = HashSet::new();
        while !vertices.is_empty() {
            let (new_vertices, new_senders) = self.do_drop_dependent_vertices(vertices);
            vertices = new_vertices;
            senders.extend(new_senders);
        }
        senders
    }

    /// Drops all vertices that have the specified direct dependencies, and returns their IDs and
    /// senders.
    fn do_drop_dependent_vertices(
        &mut self,
        vertices: Vec<Dependency<C>>,
    ) -> (Vec<Dependency<C>>, HashSet<I>) {
        // collect the vertices that depend on the ones we got in the argument and their senders
        vertices
            .into_iter()
            // filtering by is_unit, so that we don't drop vertices depending on invalid evidence
            // or endorsements - we can still get valid ones from someone else and eventually
            // satisfy the dependency
            .filter(|dep| dep.is_unit())
            .flat_map(|vertex| self.vertex_deps.remove(&vertex))
            .flatten()
            .map(|pv| (pv.pvv.inner().id(), pv.sender))
            .unzip()
    }

    /// Removes all expired entries from a `BTreeMap` of `Vec`s.
    fn remove_expired<T: Ord + Clone>(
        map: &mut BTreeMap<T, Vec<PendingVertex<I, C>>>,
        oldest: Timestamp,
    ) {
        for pvs in map.values_mut() {
            pvs.retain(|pv| !pv.expired(oldest));
        }
        let keys = map
            .iter()
            .filter(|(_, pvs)| pvs.is_empty())
            .map(|(key, _)| key.clone())
            .collect_vec();
        for key in keys {
            map.remove(&key);
        }
    }
}
