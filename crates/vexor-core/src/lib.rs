//! Pure, I/O-free core of Vexor: the per-instance lifecycle state machine and
//! the restart policy. The shim and daemon feed events in and execute the
//! returned effects; nothing in this crate touches processes, clocks, or files.
//!
//! See docs/architecture.md §4.

pub mod lifecycle;
pub mod restart;
pub mod spec;
pub mod time;

pub use lifecycle::{
    AfterStop, Ctx, Desired, Effect, ErrorReason, Event, ExitStatus, HealthPolicy, Instance,
    InstancePolicy, ReplacementReady, State, Timer,
};
pub use restart::{Backoff, Jitter, MIN_RESTART_INTERVAL, RestartPolicy};
pub use spec::{
    AppSpec, ClusterSpec, DurationUnit, HealthCheckSpec, HealthTarget, KillSignal, LogSpec,
    Overflow, PolicySpec, ReloadStrategy, RouteSpec, WatchSpec,
};
pub use time::Millis;
