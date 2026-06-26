pub mod mode;
pub mod router;

// The 5-field cron schedule source is a standalone convenience (RFC 0008); the
// production path is an external CronJob → `--mode once`. Feature-gated, no deps.
#[cfg(feature = "cron")]
pub mod timer;
