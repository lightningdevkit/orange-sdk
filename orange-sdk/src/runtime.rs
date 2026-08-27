// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use ldk_node::lightning::util::logger::Logger as _;
use ldk_node::lightning::{log_debug, log_error, log_trace, log_warn};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::{JoinHandle, JoinSet};

use crate::logging::Logger;

// The timeout after which we give up waiting on a background task to exit on shutdown.
pub(crate) const BACKGROUND_TASK_SHUTDOWN_TIMEOUT_SECS: u64 = 5;

pub(crate) struct Runtime {
	mode: RuntimeMode,
	background_tasks: Mutex<JoinSet<()>>,
	cancellable_background_tasks: Mutex<JoinSet<()>>,
	logger: Arc<Logger>,
}

impl Runtime {
	pub fn new(logger: Arc<Logger>) -> Result<Self, std::io::Error> {
		let mode = match tokio::runtime::Handle::try_current() {
			Ok(handle) => RuntimeMode::Handle(handle),
			Err(_) => {
				let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
				RuntimeMode::Owned(rt)
			},
		};
		let background_tasks = Mutex::new(JoinSet::new());
		let cancellable_background_tasks = Mutex::new(JoinSet::new());

		Ok(Self { mode, background_tasks, cancellable_background_tasks, logger })
	}

	pub fn get_handle(&self) -> tokio::runtime::Handle {
		match &self.mode {
			RuntimeMode::Owned(rt) => rt.handle().clone(),
			RuntimeMode::Handle(h) => h.clone(),
		}
	}

	#[allow(unused)]
	pub fn with_handle(handle: tokio::runtime::Handle, logger: Arc<Logger>) -> Self {
		let mode = RuntimeMode::Handle(handle);
		let background_tasks = Mutex::new(JoinSet::new());
		let cancellable_background_tasks = Mutex::new(JoinSet::new());

		Self { mode, background_tasks, cancellable_background_tasks, logger }
	}

	pub fn spawn_background_task<F>(&self, future: F)
	where
		F: Future<Output = ()> + Send + 'static,
	{
		let mut background_tasks = self.background_tasks.lock().unwrap();
		self.reap_completed_tasks(&mut background_tasks);
		let runtime_handle = self.handle();
		background_tasks.spawn_on(future, runtime_handle);
	}

	pub fn spawn_cancellable_background_task<F>(&self, future: F)
	where
		F: Future<Output = ()> + Send + 'static,
	{
		let mut cancellable_background_tasks = self.cancellable_background_tasks.lock().unwrap();
		self.reap_completed_tasks(&mut cancellable_background_tasks);
		let runtime_handle = self.handle();
		cancellable_background_tasks.spawn_on(future, runtime_handle);
	}

	#[allow(unused)]
	pub fn spawn_blocking<F, R>(&self, func: F) -> JoinHandle<R>
	where
		F: FnOnce() -> R + Send + 'static,
		R: Send + 'static,
	{
		let handle = self.handle();
		handle.spawn_blocking(func)
	}

	pub fn block_on<F: Future>(&self, future: F) -> F::Output {
		// While we generally decided not to overthink via which call graph users would enter our
		// runtime context, we'd still try to reuse whatever current context would be present
		// during `block_on`, as this is the context `block_in_place` would operate on. So we try
		// to detect the outer context here, and otherwise use whatever was set during
		// initialization.
		let handle = tokio::runtime::Handle::try_current().unwrap_or(self.handle().clone());
		tokio::task::block_in_place(move || handle.block_on(future))
	}

	pub fn abort_cancellable_background_tasks(&self) {
		let mut tasks = core::mem::take(&mut *self.cancellable_background_tasks.lock().unwrap());
		if tasks.is_empty() {
			log_warn!(self.logger, "Stopping cancellable background tasks with no tasks");
			return;
		}
		tasks.abort_all();
		self.block_on(async { while tasks.join_next().await.is_some() {} })
	}

	pub fn wait_on_background_tasks(&self) {
		let mut tasks = core::mem::take(&mut *self.background_tasks.lock().unwrap());
		if tasks.is_empty() {
			log_warn!(self.logger, "Stopping background tasks with no tasks");
			return;
		}
		self.block_on(async {
			loop {
				let timeout_fut = tokio::time::timeout(
					Duration::from_secs(BACKGROUND_TASK_SHUTDOWN_TIMEOUT_SECS),
					tasks.join_next_with_id(),
				);
				match timeout_fut.await {
					Ok(Some(Ok((id, _)))) => {
						log_trace!(self.logger, "Stopped background task with id {id}");
					},
					Ok(Some(Err(e))) => {
						tasks.abort_all();
						log_trace!(self.logger, "Stopping background task failed: {e}");
						break;
					},
					Ok(None) => {
						log_debug!(self.logger, "Stopped all background tasks");
						break;
					},
					Err(e) => {
						tasks.abort_all();
						log_error!(self.logger, "Stopping background task timed out: {e}");
						break;
					},
				}
			}
		})
	}

	fn handle(&self) -> &tokio::runtime::Handle {
		match &self.mode {
			RuntimeMode::Owned(rt) => rt.handle(),
			RuntimeMode::Handle(handle) => handle,
		}
	}

	fn reap_completed_tasks(&self, tasks: &mut JoinSet<()>) {
		while let Some(result) = tasks.try_join_next() {
			if let Err(error) = result {
				log_error!(self.logger, "Background task failed: {error}");
			}
		}
	}
}

enum RuntimeMode {
	Owned(tokio::runtime::Runtime),
	Handle(tokio::runtime::Handle),
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::logging::LoggerType;

	fn test_runtime() -> Runtime {
		let logger = Arc::new(Logger::new(&LoggerType::LogFacade).expect("logger"));
		// Share the current-thread test runtime. Spawned tasks only run while this test awaits,
		// and each task runs to completion inside one poll, so a task that has run is also
		// complete from the `JoinSet`'s point of view.
		Runtime::with_handle(tokio::runtime::Handle::current(), logger)
	}

	async fn run_spawned_tasks() {
		// Each yield lets the scheduler run the tasks that are ready. A few rounds are enough for
		// tasks that finish in one poll.
		for _ in 0..8 {
			tokio::task::yield_now().await;
		}
	}

	#[tokio::test]
	async fn spawning_reaps_completed_cancellable_tasks() {
		let runtime = test_runtime();
		let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

		for _ in 0..64 {
			let completed = Arc::clone(&completed);
			runtime.spawn_cancellable_background_task(async move {
				completed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
			});
		}
		run_spawned_tasks().await;
		assert_eq!(completed.load(std::sync::atomic::Ordering::SeqCst), 64);
		assert_eq!(runtime.cancellable_background_tasks.lock().unwrap().len(), 64);

		runtime.spawn_cancellable_background_task(async {});

		assert_eq!(runtime.cancellable_background_tasks.lock().unwrap().len(), 1);
	}

	#[tokio::test]
	async fn spawning_reaps_completed_background_tasks() {
		let runtime = test_runtime();

		for _ in 0..64 {
			runtime.spawn_background_task(async {});
		}
		run_spawned_tasks().await;
		assert_eq!(runtime.background_tasks.lock().unwrap().len(), 64);

		runtime.spawn_background_task(async {});

		assert_eq!(runtime.background_tasks.lock().unwrap().len(), 1);
	}

	#[tokio::test]
	async fn spawning_keeps_running_tasks() {
		let runtime = test_runtime();
		let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

		runtime.spawn_cancellable_background_task(async move {
			let _ = release_rx.await;
		});
		runtime.spawn_cancellable_background_task(async {});
		run_spawned_tasks().await;

		runtime.spawn_cancellable_background_task(async {});

		// The blocked task stays; the completed one is reaped; the new one is registered.
		assert_eq!(runtime.cancellable_background_tasks.lock().unwrap().len(), 2);
		drop(release_tx);
	}
}
