use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use polars_core::config;
use polars_error::PolarsResult;
use polars_io::predicates::ScanIOPredicate;
use polars_utils::IdxSize;

use crate::async_executor::AbortOnDropHandle;
use crate::async_primitives::connector::{Receiver, Sender, connector};
use crate::async_primitives::wait_group::{WaitGroup, WaitToken};
use crate::morsel::SourceToken;
use crate::nodes::compute_node_prelude::*;

pub mod multi_file_reader;

pub mod batch;
#[cfg(feature = "csv")]
pub mod csv;
#[cfg(feature = "ipc")]
pub mod ipc;
pub mod multi_scan;
#[cfg(feature = "json")]
pub mod ndjson;
#[cfg(feature = "parquet")]
pub mod parquet;

#[derive(Clone, Debug)]
pub enum RowRestriction {
    Slice(Range<usize>),
    Predicate(ScanIOPredicate),
}

/// The state needed to manage a spawned [`SourceNode`].
struct StartedSourceComputeNode {
    output_send: Sender<SourceOutput>,
    join_handles: FuturesUnordered<AbortOnDropHandle<PolarsResult<()>>>,
}

/// A [`ComputeNode`] to wrap a [`SourceNode`].
pub struct SourceComputeNode<T: SourceNode + Send + Sync> {
    source: T,
    started: Option<StartedSourceComputeNode>,
}

impl<T: SourceNode + Send + Sync> SourceComputeNode<T> {
    pub fn new(source: T) -> Self {
        Self {
            source,
            started: None,
        }
    }
}

impl<T: SourceNode> ComputeNode for SourceComputeNode<T> {
    fn name(&self) -> &str {
        self.source.name()
    }

    fn update_state(
        &mut self,
        recv: &mut [PortState],
        send: &mut [PortState],
        _state: &StreamingExecutionState,
    ) -> polars_error::PolarsResult<()> {
        assert!(recv.is_empty());
        assert_eq!(send.len(), 1);

        if self
            .started
            .as_ref()
            .is_some_and(|s| s.join_handles.is_empty())
        {
            send[0] = PortState::Done;
        }

        if send[0] != PortState::Done {
            send[0] = PortState::Ready;
        }

        Ok(())
    }

    fn spawn<'env, 's>(
        &'env mut self,
        scope: &'s super::TaskScope<'s, 'env>,
        recv_ports: &mut [Option<RecvPort<'_>>],
        send_ports: &mut [Option<SendPort<'_>>],
        state: &'s StreamingExecutionState,
        join_handles: &mut Vec<JoinHandle<PolarsResult<()>>>,
    ) {
        assert!(recv_ports.is_empty());
        assert_eq!(send_ports.len(), 1);

        let name = self.name().to_string();
        let started = self.started.get_or_insert_with(|| {
            let (tx, rx) = connector();
            let mut join_handles = Vec::new();

            self.source.spawn_source(rx, state, &mut join_handles, None);
            // One of the tasks might throw an error. In which case, we need to cancel all
            // handles and find the error.
            let join_handles: FuturesUnordered<_> =
                join_handles.drain(..).map(AbortOnDropHandle::new).collect();

            StartedSourceComputeNode {
                output_send: tx,
                join_handles,
            }
        });

        let send = send_ports[0].take().unwrap();
        let source_output = if self
            .source
            .is_source_output_parallel(send.is_receiver_serial())
        {
            SourceOutputPort::Parallel(send.parallel())
        } else {
            SourceOutputPort::Serial(send.serial())
        };
        join_handles.push(scope.spawn_task(TaskPriority::High, async move {
            let (outcome, wait_group, source_output) = SourceOutput::from_port(source_output);

            if started.output_send.send(source_output).await.is_ok() {
                // Wait for the phase to finish.
                wait_group.wait().await;
                if !outcome.did_finish() {
                    return Ok(());
                }

                if config::verbose() {
                    eprintln!("[{name}]: Last data received.");
                }
            };

            // Either the task finished or some error occurred.
            while let Some(ret) = started.join_handles.next().await {
                ret?;
            }

            Ok(())
        }));
    }
}

/// Token that contains the outcome of a phase.
///
/// Namely, this indicates whether a phase finished completely or whether it was stopped before
/// that.
#[derive(Clone)]
pub struct PhaseOutcomeToken {
    /// - `false` -> finished / panicked
    /// - `true` -> stopped before finishing
    stop: Arc<AtomicBool>,
}

impl PhaseOutcomeToken {
    pub fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Indicate that the phase was stopped before finishing.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Returns whether the phase was stopped before finishing.
    pub fn was_stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Returns whether the phase was finished completely.
    pub fn did_finish(&self) -> bool {
        !self.was_stopped()
    }
}

/// Output for a phase.
pub struct SourceOutput {
    pub outcome: PhaseOutcomeToken,
    pub port: SourceOutputPort,

    #[allow(unused)]
    /// Dropping this indicates that the phase is done.
    wait_token: WaitToken,
}

/// Output for a single morsel sender in a phase.
pub struct MorselOutput {
    pub outcome: PhaseOutcomeToken,
    pub port: Sender<Morsel>,
    pub source_token: SourceToken,

    #[allow(unused)]
    /// Dropping this indicates that the morsel sender is done.
    wait_token: WaitToken,
}

impl SourceOutput {
    pub fn from_port(port: SourceOutputPort) -> (PhaseOutcomeToken, WaitGroup, Self) {
        let outcome = PhaseOutcomeToken::new();
        let wait_group = WaitGroup::default();

        let output = Self {
            outcome: outcome.clone(),
            wait_token: wait_group.token(),
            port,
        };
        (outcome, wait_group, output)
    }
}

impl MorselOutput {
    pub fn from_port(
        port: Sender<Morsel>,
        source_token: SourceToken,
    ) -> (PhaseOutcomeToken, WaitGroup, Self) {
        let outcome = PhaseOutcomeToken::new();
        let wait_group = WaitGroup::default();

        let output = Self {
            outcome: outcome.clone(),
            wait_token: wait_group.token(),
            port,
            source_token,
        };
        (outcome, wait_group, output)
    }
}

/// The output port of a [`SourceNode`].
///
/// This is essentially an owned [`SendPort`].
pub enum SourceOutputPort {
    Serial(Sender<Morsel>),
    Parallel(Vec<Sender<Morsel>>),
}

impl SourceOutputPort {
    pub fn serial(self) -> Sender<Morsel> {
        match self {
            Self::Serial(s) => s,
            _ => panic!(),
        }
    }

    pub fn parallel(self) -> Vec<Sender<Morsel>> {
        match self {
            Self::Parallel(s) => s,
            _ => panic!(),
        }
    }
}

/// A node in the streaming physical graph that only produces [`Morsel`]s.
///
/// These can be converting into [`ComputeNode`]s that will have non-scoped tasks.
pub trait SourceNode: Sized + Send + Sync {
    fn name(&self) -> &str;

    fn is_source_output_parallel(&self, is_receiver_serial: bool) -> bool;

    /// Start all the tasks for the [`SourceNode`].
    ///
    /// This should repeatedly take a [`SourceOutput`] from `output_recv` and output its
    /// [`Morsel`] into the output's port. When a stop is requested, the output's outcome should be
    /// set to stop, the output should be dropped and the task should wait for a new output to be
    /// provided. When the source is finished, the output should also be dropped.
    ///
    /// It should produce at least one task that lives until the source is finished and all the
    /// join handles for the source tasks should be directly or indirectly awaited by awaiting all
    /// `join_handles`.
    ///
    /// If the `unfiltered_row_count` is given as `Some(..)` a scalar column is appended at the end
    /// of the dataframe that contains the unrestricted row count for each `Morsel` (i.e. the row
    /// count before slicing and predicate filtering).
    fn spawn_source(
        &mut self,
        output_recv: Receiver<SourceOutput>,
        state: &StreamingExecutionState,
        join_handles: &mut Vec<JoinHandle<PolarsResult<()>>>,
        unrestricted_row_count: Option<tokio::sync::oneshot::Sender<IdxSize>>,
    );
}
