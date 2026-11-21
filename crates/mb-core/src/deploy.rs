//! Executing a plan: test deploy (rollback) and real deploy.

use std::time::Instant;

use crate::conn::{connect, ConnParams, SessionOptions};
use crate::error::{Error, PgResultExt, Result};
use crate::plan::{Plan, RunMode};
use crate::progress::{cancellable, CancellationToken};

#[derive(Clone, Debug)]
pub struct DeployOptions {
    /// Execute and roll back (validation only).
    pub test_only: bool,
    /// Run in-transaction steps in one transaction (otherwise each step autocommits).
    pub single_transaction: bool,
    pub lock_timeout_secs: u64,
}

impl Default for DeployOptions {
    fn default() -> Self {
        Self { test_only: false, single_transaction: true, lock_timeout_secs: 10 }
    }
}

#[derive(Clone, Debug)]
pub enum DeployProgress {
    Started { index: usize, total: usize },
    Finished { index: usize, millis: u128 },
    Skipped { index: usize, reason: String },
}

#[derive(Clone, Debug, Default)]
pub struct DeployReport {
    pub executed: usize,
    pub skipped: usize,
    pub failed_step: Option<usize>,
    pub error: Option<String>,
    pub rolled_back: bool,
    pub committed: bool,
    pub millis: u128,
}

impl DeployReport {
    pub fn ok(&self) -> bool {
        self.failed_step.is_none() && self.error.is_none()
    }
}

/// Execute a plan against the target.
pub async fn deploy(
    target: &ConnParams,
    plan: &Plan,
    opts: &DeployOptions,
    cancel: &CancellationToken,
    progress: &(dyn Fn(DeployProgress) + Send + Sync),
) -> Result<DeployReport> {
    let t0 = Instant::now();
    let client = connect(target, &SessionOptions::deploy(opts.lock_timeout_secs)).await?;
    client
        .batch_execute("SET check_function_bodies = off;")
        .await
        .sql("SET check_function_bodies = off")?;

    let total = plan.steps.len();
    let mut report = DeployReport::default();

    // Partition steps by run mode, preserving order.
    let order: Vec<usize> = if opts.test_only {
        (0..total).collect()
    } else {
        let mut v: Vec<usize> = (0..total).filter(|&i| plan.steps[i].run == RunMode::BeforeTransaction).collect();
        v.extend((0..total).filter(|&i| plan.steps[i].run == RunMode::InTransaction));
        v.extend((0..total).filter(|&i| plan.steps[i].run == RunMode::AfterTransaction));
        v
    };

    let mut in_tx = false;
    let mut error: Option<(usize, Error)> = None;

    for &i in &order {
        let step = &plan.steps[i];
        if step.is_manual() {
            report.skipped += 1;
            progress(DeployProgress::Skipped { index: i, reason: "manual step".into() });
            continue;
        }
        if opts.test_only && step.run == RunMode::AfterTransaction {
            report.skipped += 1;
            progress(DeployProgress::Skipped { index: i, reason: "cannot run inside a transaction; not tested".into() });
            continue;
        }
        let want_tx = opts.test_only || (opts.single_transaction && step.run == RunMode::InTransaction);
        if want_tx && !in_tx {
            client.batch_execute("BEGIN").await.sql("BEGIN")?;
            in_tx = true;
        } else if !want_tx && in_tx {
            client.batch_execute("COMMIT").await.sql("COMMIT")?;
            in_tx = false;
            report.committed = true;
        }
        progress(DeployProgress::Started { index: i, total });
        let ts = Instant::now();
        let res = cancellable(cancel, async { client.batch_execute(&step.sql).await.sql(&step.sql) }).await;
        match res {
            Ok(()) => {
                report.executed += 1;
                progress(DeployProgress::Finished { index: i, millis: ts.elapsed().as_millis() });
            }
            Err(e) => {
                if matches!(e, Error::Cancelled) {
                    // Ask the server to stop the running statement.
                    let _ = crate::conn::cancel_running(&client, target).await;
                }
                error = Some((i, e));
                break;
            }
        }
    }

    if in_tx {
        if opts.test_only || error.is_some() {
            let _ = client.batch_execute("ROLLBACK").await;
            report.rolled_back = true;
        } else {
            client.batch_execute("COMMIT").await.sql("COMMIT")?;
            report.committed = true;
        }
    }
    if let Some((i, e)) = error {
        report.failed_step = Some(i);
        report.error = Some(e.to_string());
    }
    report.millis = t0.elapsed().as_millis();
    Ok(report)
}
