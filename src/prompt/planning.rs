use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::environment::PromptEnvironment;
use crate::gitstatus;
use crate::runtime::{self, Runtime};

/// Shared by every generation in one client. A canceled await does not cancel
/// kernel filesystem I/O: the worker owns its permit until it really finishes.
#[derive(Clone)]
pub(super) struct PlanningExecutor(Arc<Semaphore>);

impl PlanningExecutor {
    pub(super) fn new() -> Self {
        Self(Arc::new(Semaphore::new(2)))
    }

    async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> io::Result<T> {
        let permit = Arc::clone(&self.0)
            .acquire_owned()
            .await
            .expect("the planning semaphore is never closed");
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work()
        })
        .await
        .map_err(|error| io::Error::other(format!("prompt planning task failed: {error}")))
    }

    pub(super) async fn git_query(
        &self,
        cwd: PathBuf,
        environment: Arc<PromptEnvironment>,
    ) -> io::Result<Option<gitstatus::Query>> {
        if let Some(query) = gitstatus::Query::explicit(&cwd, &environment)? {
            return Ok(Some(query));
        }
        self.run(move || gitstatus::Query::discover(&cwd, &environment))
            .await
    }
}

/// Immutable request-owned inputs; neither cancellation nor a later request
/// can change the environment observed by a worker.
#[derive(Clone)]
pub(super) struct PlanningRequest {
    pub(super) cwd: PathBuf,
    pub(super) active: Vec<Runtime>,
    pub(super) environment: Arc<PromptEnvironment>,
    pub(super) executor: PlanningExecutor,
}

impl PlanningRequest {
    pub(super) async fn refresh(
        &self,
        runtime: Runtime,
    ) -> io::Result<runtime::cache::RuntimePlan> {
        let request = self.clone();
        self.executor
            .run(move || runtime::cache::refresh_plan(runtime, &request.cwd, &request.environment))
            .await
    }

    pub(super) async fn build(&self) -> io::Result<Vec<runtime::cache::RuntimePlan>> {
        let request = self.clone();
        self.executor
            .run(move || {
                let Self {
                    cwd,
                    active,
                    environment,
                    ..
                } = request;
                let git_root = runtime::detect::worktree_root(&cwd, &environment);
                let project =
                    runtime::detect::detect(&cwd, git_root.as_deref(), &active, &environment);
                // One bounded job covers all filesystem work, including
                // selector reads and finalization. Detection first also avoids
                // resolving executables for runtimes absent from this project.
                let base =
                    runtime::cache::resolve_base_plans(&project.runtimes, &cwd, &environment);
                runtime::cache::finalize_plans(&cwd, &project, base, &environment)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;
    use tokio::time::{Instant, timeout};

    use super::PlanningExecutor;
    use crate::{
        daemon, environment::PromptEnvironment, prompt, runtime::Runtime, theme::AsyncTheme,
    };
    use std::sync::Arc;

    async fn blocked_job(
        executor: PlanningExecutor,
    ) -> (
        JoinHandle<std::io::Result<()>>,
        mpsc::Sender<()>,
        oneshot::Receiver<()>,
    ) {
        let (release, blocked) = mpsc::channel();
        let (started, ready) = oneshot::channel();
        let (finished, done) = oneshot::channel();
        let task = tokio::spawn(async move {
            executor
                .run(move || {
                    started.send(()).unwrap();
                    // Dropping the release sender also unblocks this test worker
                    // on assertion failure, so test runtime shutdown cannot hang.
                    let _ = blocked.recv();
                    let _ = finished.send(());
                })
                .await
        });
        timeout(Duration::from_secs(1), ready)
            .await
            .unwrap()
            .unwrap();
        (task, release, done)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_planning_keeps_deadlines_and_cancellation_responsive_and_jobs_bounded() {
        let executor = PlanningExecutor::new();
        let (first, release_first, first_done) = blocked_job(executor.clone()).await;
        let (second, release_second, second_done) = blocked_job(executor.clone()).await;
        first.abort();
        second.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert!(second.await.unwrap_err().is_cancelled());
        assert_eq!(executor.0.available_permits(), 0);

        // Automatic Git discovery shares the bounded filesystem pool.
        assert!(
            timeout(
                Duration::from_millis(30),
                executor.git_query(std::env::temp_dir(), Arc::new(PromptEnvironment::default())),
            )
            .await
            .is_err()
        );
        // Pure explicit selections must remain responsive even when both
        // filesystem workers are stalled and no permits are available.
        for (git_dir, worktree, expected, is_git_dir) in [
            (Some("/explicit/repo.git"), None, "/explicit/repo.git", true),
            (
                None,
                Some("/explicit/checkout"),
                "/explicit/checkout",
                false,
            ),
        ] {
            let environment = Arc::new(PromptEnvironment {
                git_dir: git_dir.map(Into::into),
                git_work_tree: worktree.map(Into::into),
                git_ceilings: Some("/nonexistent".into()),
                ..PromptEnvironment::default()
            });
            let query = timeout(
                Duration::from_millis(100),
                executor.git_query("/nonexistent/cwd".into(), environment),
            )
            .await
            .unwrap()
            .unwrap()
            .unwrap();
            assert_eq!(query.path(), std::path::Path::new(expected));
            assert_eq!(query.is_git_dir(), is_git_dir);
        }

        // Minimal compiled theme: Rust with ten empty rendering strings.
        let theme = Arc::new(
            AsyncTheme::decode_hex(&format!(
                "0001{:02x}{}",
                Runtime::Rust.id(),
                "0000".repeat(10),
            ))
            .unwrap(),
        );
        assert_eq!(theme.runtimes(), vec![Runtime::Rust]);
        let snapshot = |generation| {
            let executor = executor.clone();
            let theme = Arc::clone(&theme);
            async move {
                prompt::snapshot(
                    generation,
                    std::env::temp_dir(),
                    daemon::Instance::Development("blocked-planning-test".to_owned()),
                    Arc::new(PromptEnvironment::default()),
                    &theme,
                    executor,
                )
                .await
            }
        };

        let started = Instant::now();
        timeout(Duration::from_millis(900), snapshot(1))
            .await
            .unwrap()
            .unwrap();
        assert!(started.elapsed() >= prompt::REQUEST_TIMEOUT);
        // A newer request can cancel a snapshot waiting for a permit without
        // releasing the permits retained by either real blocking worker.
        let superseded = tokio::spawn(snapshot(2));
        tokio::task::yield_now().await;
        superseded.abort();
        timeout(Duration::from_millis(100), superseded)
            .await
            .unwrap()
            .unwrap_err();
        timeout(Duration::from_millis(900), snapshot(3))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(executor.0.available_permits(), 0);

        // No third blocking closure may start even after canceled generations.
        let pending = executor.run(|| panic!("a third planning worker started"));
        assert!(timeout(Duration::from_millis(30), pending).await.is_err());
        release_first.send(()).unwrap();
        first_done.await.unwrap();
        assert_eq!(executor.run(|| 42).await.unwrap(), 42);
        release_second.send(()).unwrap();
        second_done.await.unwrap();
    }
}
