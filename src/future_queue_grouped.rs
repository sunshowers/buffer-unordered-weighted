// Copyright (c) The buffer-unordered-weighted Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::peekable_fused::PeekableFused;
use futures_util::{
    ready,
    stream::{Fuse, FusedStream, FuturesUnordered},
    Future, Stream, StreamExt,
};
use pin_project_lite::pin_project;
use std::{
    borrow::Borrow,
    collections::{HashMap, VecDeque},
    fmt,
    hash::Hash,
    pin::Pin,
    task::{Context, Poll},
};

pin_project! {
    /// Stream for the [`future_queue_grouped`](crate::StreamExt::future_queue_grouped) method.
    #[must_use = "streams do nothing unless polled"]
    pub struct FutureQueueGrouped<St, K>
    where
        St: Stream,
        St::Item: GroupedWeightedFuture,
     {
        #[pin]
        stream: Fuse<St>,
        #[pin]
        in_progress_queue: PeekableFused<
            FuturesUnordered<
                FutureWithGW<
                    <St::Item as GroupedWeightedFuture>::Future,
                    <St::Item as GroupedWeightedFuture>::Q
                >
            >
        >,
        max_global_weight: usize,
        current_global_weight: usize,
        group_store: GroupStore<<St::Item as GroupedWeightedFuture>::Q, K, <St::Item as GroupedWeightedFuture>::Future>,
    }
}

impl<St, K> fmt::Debug for FutureQueueGrouped<St, K>
where
    St: Stream + fmt::Debug,
    St::Item: GroupedWeightedFuture,
    <St::Item as GroupedWeightedFuture>::Future: fmt::Debug,
    <<St::Item as GroupedWeightedFuture>::Future as Future>::Output: fmt::Debug,
    K: fmt::Debug,
    <St::Item as GroupedWeightedFuture>::Q: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FutureQueueGrouped")
            .field("stream", &self.stream)
            .field("in_progress_queue", &self.in_progress_queue)
            .field("max_global_weight", &self.max_global_weight)
            .field("current_global_weight", &self.current_global_weight)
            .field("group_store", &self.group_store)
            .finish()
    }
}

impl<St, K> FutureQueueGrouped<St, K>
where
    St: Stream,
    St::Item: GroupedWeightedFuture,
    <St::Item as GroupedWeightedFuture>::Q: Eq + Hash + fmt::Debug,
    K: Eq + Hash + fmt::Debug + Borrow<<St::Item as GroupedWeightedFuture>::Q>,
{
    pub(super) fn new(
        stream: St,
        max_global_weight: usize,
        id_data: impl IntoIterator<Item = (K, usize)>,
    ) -> Self {
        let id_data_store = GroupStore::new(id_data);
        Self {
            stream: stream.fuse(),
            in_progress_queue: PeekableFused::new(FuturesUnordered::new()),
            max_global_weight,
            current_global_weight: 0,
            group_store: id_data_store,
        }
    }

    /// Returns the maximum weight of futures allowed to be run by this adaptor.
    pub fn max_global_weight(&self) -> usize {
        self.max_global_weight
    }

    /// Returns the current global weight of futures.
    pub fn current_global_weight(&self) -> usize {
        self.current_global_weight
    }

    /// Returns the maximum weight of futures allowed to be run within this group.
    pub fn max_group_weight<Q>(&self, id: &Q) -> Option<usize>
    where
        Q: Eq + Hash + fmt::Debug + ?Sized,
        K: Borrow<Q>,
    {
        self.group_store
            .group_data
            .get(id)
            .map(|id_data| id_data.max_weight)
    }

    /// Returns the current weight of futures being run within this group.
    pub fn current_group_weight<Q>(&self, id: &Q) -> Option<usize>
    where
        Q: Eq + Hash + fmt::Debug + ?Sized,
        K: Borrow<Q>,
    {
        self.group_store
            .group_data
            .get(id)
            .map(|id_data| id_data.max_weight)
    }

    /// Acquires a reference to the underlying sink or stream that this combinator is
    /// pulling from.
    pub fn get_ref(&self) -> &St {
        self.stream.get_ref()
    }

    /// Acquires a mutable reference to the underlying sink or stream that this
    /// combinator is pulling from.
    ///
    /// Note that care must be taken to avoid tampering with the state of the
    /// sink or stream which may otherwise confuse this combinator.
    pub fn get_mut(&mut self) -> &mut St {
        self.stream.get_mut()
    }

    /// Acquires a pinned mutable reference to the underlying sink or stream that this
    /// combinator is pulling from.
    ///
    /// Note that care must be taken to avoid tampering with the state of the
    /// sink or stream which may otherwise confuse this combinator.
    pub fn get_pin_mut(self: Pin<&mut Self>) -> core::pin::Pin<&mut St> {
        self.project().stream.get_pin_mut()
    }

    /// Consumes this combinator, returning the underlying sink or stream.
    ///
    /// Note that this may discard intermediate state of this combinator, so
    /// care should be taken to avoid losing resources when this is called.
    pub fn into_inner(self) -> St {
        self.stream.into_inner()
    }

    // ---
    // Helper methods
    // ---

    // This returns true if any new futures were added to the in_progress_queue.
    fn poll_pop_in_progress(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<(
        Option<<<St::Item as GroupedWeightedFuture>::Future as Future>::Output>,
        bool,
    )> {
        let mut this = self.project();

        match ready!(this.in_progress_queue.poll_next_unpin(cx)) {
            Some((weight, id, output)) => {
                *this.current_global_weight = this
                    .current_global_weight
                    .checked_sub(weight)
                    .unwrap_or_else(|| {
                        panic!(
                        "future_queue_grouped: subtracted weight {} from current {}, overflowed",
                        weight,
                        this.current_global_weight,
                    )
                    });

                let mut any_queued = false;

                if let Some(id) = id {
                    let data = this.group_store.get_id_mut_or_unwrap(&id);
                    data.sub_weight(&id, weight);

                    // Can we queue up additional futures from the queued ones for this ID?
                    while data.current_weight < data.max_weight
                        && this.current_global_weight < this.max_global_weight
                    {
                        let (weight, id, future) = match data.queued.pop_front() {
                            Some(x) => x,
                            None => break,
                        };
                        data.add_weight(&id, weight);
                        *this.current_global_weight = this
                            .current_global_weight
                            .checked_add(weight)
                            .unwrap_or_else(|| {
                                panic!(
                                "future_queue_grouped: added weight {} to current {}, overflowed",
                                weight,
                                this.current_global_weight,
                            )
                            });
                        this.in_progress_queue
                            .as_mut()
                            .get_pin_mut()
                            .push(FutureWithGW::new(weight, Some(id), future));
                        any_queued = true;
                    }
                }

                Poll::Ready((Some(output), any_queued))
            }
            None => Poll::Ready((None, false)),
        }
    }
}

impl<St, K> Stream for FutureQueueGrouped<St, K>
where
    St: Stream,
    St::Item: GroupedWeightedFuture,
    <St::Item as GroupedWeightedFuture>::Q: Eq + Hash + fmt::Debug,
    K: Eq + Hash + fmt::Debug + Borrow<<St::Item as GroupedWeightedFuture>::Q>,
{
    type Item = <<St::Item as GroupedWeightedFuture>::Future as Future>::Output;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // First, attempt to pull the next value from the in progress queue.
        let (return_output, mut any_queued) = ready!(self.as_mut().poll_pop_in_progress(cx));

        let mut this = self.as_mut().project();

        // Next, let's try to spawn off as many futures as possible by filling up our queue of
        // futures.

        while *this.current_global_weight < *this.max_global_weight {
            match this.stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(weighted_future)) => {
                    let (weight, id, future) = weighted_future.into_components();

                    if let Some(id) = id {
                        // Is this group full?
                        let data = this.group_store.get_id_mut_or_unwrap(&id);
                        // Just like for the global weight, we want to allow exceeding the weight
                        // but stop running subsequent futures after that. This prevents issues
                        // where a particular future requires more than the maximum weight available
                        // for that ID.
                        if data.current_weight < data.max_weight {
                            data.add_weight(&id, weight);
                            *this.current_global_weight = this.current_global_weight.checked_add(weight).unwrap_or_else(|| {
                                panic!(
                                    "future_queue_grouped: added weight {} to current {}, overflowed",
                                    weight,
                                    this.current_global_weight,
                                )
                            });
                            this.in_progress_queue
                                .as_mut()
                                .get_pin_mut()
                                .push(FutureWithGW::new(weight, Some(id), future));
                            any_queued = true;
                        } else {
                            data.queued.push_back((weight, id, future));
                        }
                    } else {
                        // No ID associated with this future.
                        *this.current_global_weight =
                            this.current_global_weight.checked_add(weight).unwrap_or_else(|| {
                                panic!(
                                    "future_queue_grouped: added weight {} to current {}, overflowed",
                                    weight,
                                    this.current_global_weight,
                                )
                            });
                        this.in_progress_queue
                            .as_mut()
                            .get_pin_mut()
                            .push(FutureWithGW::new(weight, None, future));
                        any_queued = true;
                    }
                }
                Poll::Ready(None) | Poll::Pending => break,
            }
        }

        if any_queued {
            // Start any futures that were just queued up. If this returns Pending, then that's fine --
            // the task will be scheduled on the waker.
            let _ = this.in_progress_queue.as_mut().poll_peek(cx);
        }

        if let Some(output) = return_output {
            // A value was returned from the in-progress queue.
            Poll::Ready(Some(output))
        } else {
            match (
                self.stream.is_done(),
                self.in_progress_queue.is_terminated(),
            ) {
                (true, true) => {
                    // No more futures left to schedule. (Note that poll_pop_in_progress would have
                    // drained all futures in any queue.)
                    debug_assert_eq!(
                        self.group_store.num_queued_futures(),
                        0,
                        "no futures should be left in the queue"
                    );
                    Poll::Ready(None)
                }
                (false, true) => {
                    // The in-progress queue is empty, but the stream is still pending.
                    // (Note that Poll::Pending is OK to return here because this can only happen in
                    // the Poll::Pending case above.)
                    Poll::Pending
                }
                (_, false) => {
                    // There are still futures in the in-progress queue. We need to poll the
                    // in-progress queue to start any futures in it.
                    let (output, any_queued) = ready!(self.as_mut().poll_pop_in_progress(cx));
                    if any_queued {
                        // It's possible that poll_pop_in_progress might have added more futures to the queue.
                        let this = self.project();
                        let _ = this.in_progress_queue.poll_peek(cx);
                    }
                    Poll::Ready(output)
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // The minimum size is the in progress queue + any queued futures.
        let queue_len =
            self.in_progress_queue.size_hint().0 + self.group_store.num_queued_futures();
        let (lower, upper) = self.stream.size_hint();
        let lower = lower.saturating_add(queue_len);
        let upper = match upper {
            Some(x) => x.checked_add(queue_len),
            None => None,
        };
        (lower, upper)
    }
}

#[derive(Debug)]
struct GroupStore<Q, K, Fut> {
    group_data: HashMap<K, GroupData<Q, Fut>>,
}

impl<Q, K, Fut> GroupStore<Q, K, Fut>
where
    Q: Hash + Eq + fmt::Debug,
    K: Eq + Hash + fmt::Debug + Borrow<Q>,
{
    fn new(ids: impl IntoIterator<Item = (K, usize)>) -> Self {
        let id_data = ids
            .into_iter()
            .map(|(id, weight)| {
                let data = GroupData {
                    current_weight: 0,
                    max_weight: weight,
                    queued: VecDeque::new(),
                };
                (id, data)
            })
            .collect();

        Self {
            group_data: id_data,
        }
    }

    fn get_id_mut_or_unwrap(&mut self, id: &Q) -> &mut GroupData<Q, Fut> {
        if self.group_data.get(id).is_some() {
            // Can't just use get_mut above because we're going to run into
            // https://doc.rust-lang.org/nomicon/lifetime-mismatch.html#improperly-reduced-borrows
            // with the else branch.
            self.group_data.get_mut(id).unwrap()
        } else {
            panic!(
                "unknown semaphore ID: {:?} (known IDs: {:?})",
                id,
                self.group_data.keys()
            );
        }
    }

    fn num_queued_futures(&self) -> usize {
        self.group_data.values().map(|data| data.queued.len()).sum()
    }
}

#[derive(Debug)]
struct GroupData<Q, Fut> {
    current_weight: usize,
    max_weight: usize,
    queued: VecDeque<(usize, Q, Fut)>,
}

impl<Q: fmt::Debug, Fut> GroupData<Q, Fut> {
    // The ID is passed in only for its Debug impl.
    fn add_weight(&mut self, id: &Q, weight: usize) {
        self.current_weight = self.current_weight.checked_add(weight).unwrap_or_else(|| {
            panic!(
                "future_queue_grouped: for id `{:?}`, added weight {} to current {}, overflowed",
                id, weight, self.current_weight,
            )
        });
    }

    fn sub_weight(&mut self, id: &Q, weight: usize) {
        self.current_weight = self.current_weight.checked_sub(weight).unwrap_or_else(|| {
            panic!(
                "future_queue_grouped: for id `{:?}`, sub weight {} from current {}, underflowed",
                id, weight, self.current_weight,
            )
        });
    }
}

pin_project! {
    #[must_use = "futures do nothing unless polled"]
    struct FutureWithGW<Fut, Q> {
        #[pin]
        future: Fut,
        weight: usize,
        id: Option<Q>,
    }
}

impl<Fut, Q> FutureWithGW<Fut, Q> {
    pub fn new(weight: usize, id: Option<Q>, future: Fut) -> Self {
        Self { future, weight, id }
    }
}

impl<Fut, Q> Future for FutureWithGW<Fut, Q>
where
    Fut: Future,
{
    type Output = (usize, Option<Q>, Fut::Output);
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        match this.future.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(output) => Poll::Ready((*this.weight, this.id.take(), output)),
        }
    }
}

/// A trait for types which can be converted into a `Future`, an optional group, and a weight.
///
/// Provided in case it's necessary. This trait is only implemented for `(usize, Option<Q>, impl Future)`.
pub trait GroupedWeightedFuture: private::Sealed {
    /// The associated `Future` type.
    type Future: Future;

    /// The associated key lookup type.
    type Q;

    /// Turns self into its components.
    fn into_components(self) -> (usize, Option<Self::Q>, Self::Future);
}

impl<Fut, Q> private::Sealed for (usize, Option<Q>, Fut) where Fut: Future {}

impl<Fut, Q> GroupedWeightedFuture for (usize, Option<Q>, Fut)
where
    Fut: Future,
{
    type Future = Fut;
    type Q = Q;

    #[inline]
    fn into_components(self) -> (usize, Option<Self::Q>, Self::Future) {
        self
    }
}

pub(crate) mod private {
    pub trait Sealed {}
}
