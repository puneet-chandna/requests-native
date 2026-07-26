use std::fmt;
use std::future::Future;
use std::io;
use std::mem;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};

use tokio::runtime::{Handle, Runtime};
use tokio::task::AbortHandle;

static PROCESS_RUNTIME: OnceLock<DriverRegistry> = OnceLock::new();

#[derive(Clone)]
pub struct BlockingRuntimeDriver {
    inner: Arc<DriverInner>,
}

impl BlockingRuntimeDriver {
    pub fn process_local() -> io::Result<Self> {
        PROCESS_RUNTIME
            .get_or_init(DriverRegistry::default)
            .driver_for(std::process::id())
    }

    pub fn process_id(&self) -> u32 {
        self.inner.process_id
    }

    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    pub fn is_same_runtime(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn submit<F>(&self, future: F) -> Result<BlockingSubmission<F::Output>, BlockingDriverError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.submit_for_process(future, std::process::id())
    }

    fn submit_for_process<F>(
        &self,
        future: F,
        current_process_id: u32,
    ) -> Result<BlockingSubmission<F::Output>, BlockingDriverError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        if current_process_id != self.process_id() {
            return Err(BlockingDriverError::StaleProcess {
                recorded: self.process_id(),
                current: current_process_id,
            });
        }
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let task = self.inner.handle.spawn(async move {
            let output = future.await;
            let _ = sender.send(output);
        });
        Ok(BlockingSubmission {
            process_id: self.process_id(),
            receiver: Some(receiver),
            abort: Some(task.abort_handle()),
            cancelled,
        })
    }
}

impl fmt::Debug for BlockingRuntimeDriver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BlockingRuntimeDriver")
            .field("process_id", &self.process_id())
            .field("generation", &self.generation())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BlockingDriverError {
    StaleProcess { recorded: u32, current: u32 },
}

impl fmt::Display for BlockingDriverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleProcess { recorded, current } => write!(
                formatter,
                "blocking runtime belongs to process {recorded}, not current process {current}"
            ),
        }
    }
}

impl std::error::Error for BlockingDriverError {}

#[derive(Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BlockingTaskError {
    Cancelled,
    WorkerStopped,
    AlreadyCompleted,
    StaleProcess { recorded: u32, current: u32 },
}

impl fmt::Display for BlockingTaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("blocking runtime task was cancelled"),
            Self::WorkerStopped => formatter.write_str("blocking runtime task stopped"),
            Self::AlreadyCompleted => {
                formatter.write_str("blocking runtime task result was already consumed")
            }
            Self::StaleProcess { recorded, current } => write!(
                formatter,
                "blocking runtime task belongs to process {recorded}, not current process {current}"
            ),
        }
    }
}

impl std::error::Error for BlockingTaskError {}

pub struct BlockingSubmission<T> {
    process_id: u32,
    receiver: Option<mpsc::Receiver<T>>,
    abort: Option<AbortHandle>,
    cancelled: Arc<AtomicBool>,
}

impl<T> BlockingSubmission<T> {
    pub fn try_wait(&mut self) -> Result<Option<T>, BlockingTaskError> {
        self.try_wait_for_process(std::process::id())
    }

    fn try_wait_for_process(
        &mut self,
        current_process_id: u32,
    ) -> Result<Option<T>, BlockingTaskError> {
        self.validate_process(current_process_id)?;
        let Some(receiver) = self.receiver.as_ref() else {
            return Err(BlockingTaskError::AlreadyCompleted);
        };
        match receiver.try_recv() {
            Ok(output) => {
                self.receiver = None;
                Ok(Some(output))
            }
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => {
                self.receiver = None;
                Err(self.stopped_error())
            }
        }
    }

    pub fn wait(self) -> Result<T, BlockingTaskError> {
        self.wait_for_process(std::process::id())
    }

    fn wait_for_process(mut self, current_process_id: u32) -> Result<T, BlockingTaskError> {
        self.validate_process(current_process_id)?;
        let receiver = self
            .receiver
            .take()
            .ok_or(BlockingTaskError::AlreadyCompleted)?;
        receiver.recv().map_err(|_| self.stopped_error())
    }

    pub fn cancel(&self) -> Result<(), BlockingTaskError> {
        self.cancel_for_process(std::process::id())
    }

    fn cancel_for_process(&self, current_process_id: u32) -> Result<(), BlockingTaskError> {
        self.validate_process(current_process_id)?;
        if self.receiver.is_none() {
            return Err(BlockingTaskError::AlreadyCompleted);
        }
        self.cancelled.store(true, Ordering::Release);
        if let Some(abort) = self.abort.as_ref() {
            abort.abort();
        }
        Ok(())
    }

    fn validate_process(&self, current_process_id: u32) -> Result<(), BlockingTaskError> {
        if current_process_id == self.process_id {
            Ok(())
        } else {
            Err(BlockingTaskError::StaleProcess {
                recorded: self.process_id,
                current: current_process_id,
            })
        }
    }

    fn finish_drop_for_process(&mut self, current_process_id: u32) -> bool {
        let Some(abort) = self.abort.take() else {
            return false;
        };
        if current_process_id == self.process_id {
            abort.abort();
            true
        } else {
            mem::forget(abort);
            false
        }
    }

    fn stopped_error(&self) -> BlockingTaskError {
        if self.cancelled.load(Ordering::Acquire) {
            BlockingTaskError::Cancelled
        } else {
            BlockingTaskError::WorkerStopped
        }
    }
}

impl<T> Drop for BlockingSubmission<T> {
    fn drop(&mut self) {
        self.finish_drop_for_process(std::process::id());
    }
}

struct DriverInner {
    process_id: u32,
    generation: u64,
    handle: Handle,
    _runtime: RuntimeOwner,
}

struct RuntimeOwner {
    runtime: Option<Runtime>,
    process_id: u32,
}

impl Drop for RuntimeOwner {
    fn drop(&mut self) {
        if std::process::id() != self.process_id
            && let Some(runtime) = self.runtime.take()
        {
            mem::forget(runtime);
        }
    }
}

struct DriverRegistry {
    current: Mutex<Option<Arc<DriverInner>>>,
    next_generation: AtomicU64,
}

impl Default for DriverRegistry {
    fn default() -> Self {
        Self {
            current: Mutex::new(None),
            next_generation: AtomicU64::new(0),
        }
    }
}

impl DriverRegistry {
    fn driver_for(&self, process_id: u32) -> io::Result<BlockingRuntimeDriver> {
        let mut current = self
            .current
            .lock()
            .map_err(|_| io::Error::other("blocking runtime registry lock poisoned"))?;
        if let Some(driver) = current.as_ref()
            && driver.process_id == process_id
        {
            return Ok(BlockingRuntimeDriver {
                inner: Arc::clone(driver),
            });
        }
        if let Some(inherited) = current.take() {
            mem::forget(inherited);
        }

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_time()
            .thread_name("requests-runtime")
            .build()?;
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let driver = Arc::new(DriverInner {
            process_id,
            generation,
            handle: runtime.handle().clone(),
            _runtime: RuntimeOwner {
                runtime: Some(runtime),
                process_id,
            },
        });
        *current = Some(Arc::clone(&driver));
        Ok(BlockingRuntimeDriver { inner: driver })
    }
}

#[cfg(test)]
mod tests {
    use std::future;
    use std::io::{self, Read};
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use tokio::io::AsyncWriteExt;

    use super::{
        BlockingDriverError, BlockingRuntimeDriver, BlockingSubmission, BlockingTaskError,
        DriverRegistry,
    };

    #[derive(Debug, Eq, PartialEq)]
    struct FutureOutput {
        value: String,
        worker: thread::ThreadId,
    }

    #[test]
    fn process_local_driver_is_reused_and_runs_generic_futures_off_caller_thread() {
        let caller = thread::current().id();
        let first = BlockingRuntimeDriver::process_local().unwrap();
        let second = BlockingRuntimeDriver::process_local().unwrap();

        assert_eq!(first.process_id(), std::process::id());
        assert_eq!(first.generation(), second.generation());
        assert!(first.is_same_runtime(&second));

        let output = first
            .submit(async move {
                FutureOutput {
                    value: String::from("complete"),
                    worker: thread::current().id(),
                }
            })
            .unwrap()
            .wait()
            .unwrap();

        assert_eq!(output.value, "complete");
        assert_ne!(output.worker, caller);
    }

    #[test]
    fn process_local_runtime_drives_tokio_tcp_io() {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || -> io::Result<[u8; 4]> {
            let deadline = Instant::now() + Duration::from_secs(1);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "timed out accepting loopback connection",
                            ));
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => return Err(error),
                }
            };
            stream.set_read_timeout(Some(Duration::from_secs(1)))?;
            let mut request = [0; 4];
            stream.read_exact(&mut request)?;
            Ok(request)
        });

        let client_result = BlockingRuntimeDriver::process_local()
            .unwrap()
            .submit(async move {
                let mut stream = tokio::net::TcpStream::connect(address).await?;
                stream.write_all(b"ping").await
            })
            .unwrap()
            .wait();
        let server_result = server.join().expect("loopback server thread panicked");

        client_result
            .expect("process runtime worker stopped")
            .expect("Tokio TCP client failed");
        assert_eq!(server_result.expect("loopback server failed"), *b"ping");
    }

    #[test]
    fn submission_wait_yields_a_multi_thread_runtime_worker() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .unwrap();
        let (sender, receiver) = mpsc::channel();
        let watchdog_sender = sender.clone();
        let watchdog = thread::spawn(move || {
            thread::sleep(Duration::from_secs(1));
            let _ = watchdog_sender.send("watchdog");
        });

        let result = runtime.block_on(async move {
            tokio::spawn(async move {
                let _runtime_task = tokio::spawn(async move {
                    let _ = sender.send("runtime");
                });
                BlockingRuntimeDriver::process_local()
                    .unwrap()
                    .submit(async move { receiver.recv().unwrap() })
                    .unwrap()
                    .wait()
                    .unwrap()
            })
            .await
            .unwrap()
        });
        watchdog.join().unwrap();

        assert_eq!(result, "runtime");
    }

    #[test]
    fn submission_supports_try_wait_wait_and_cancel() {
        let driver = BlockingRuntimeDriver::process_local().unwrap();
        let mut ready = driver.submit(async { 42_u16 }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let value = loop {
            if let Some(value) = ready.try_wait().unwrap() {
                break value;
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        };
        assert_eq!(value, 42);

        let mut pending = driver.submit(future::pending::<()>()).unwrap();
        assert_eq!(pending.try_wait().unwrap(), None);
        pending.cancel().unwrap();
        assert_eq!(pending.wait(), Err(BlockingTaskError::Cancelled));
    }

    #[test]
    fn cancel_rejects_a_submission_after_its_output_was_consumed() {
        let driver = BlockingRuntimeDriver::process_local().unwrap();
        let mut submission = driver.submit(async { 42_u16 }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if submission.try_wait().unwrap().is_some() {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }

        assert_eq!(
            submission.cancel(),
            Err(BlockingTaskError::AlreadyCompleted)
        );
    }

    #[test]
    fn blocking_wait_does_not_enter_the_callers_tokio_runtime() {
        let driver = BlockingRuntimeDriver::process_local().unwrap();
        let caller_runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        let value = caller_runtime.block_on(async move {
            driver
                .submit(async { String::from("no nested block_on") })
                .unwrap()
                .wait()
                .unwrap()
        });

        assert_eq!(value, "no nested block_on");
    }

    #[test]
    fn registry_releases_its_lock_before_submitted_work_runs() {
        let registry = Arc::new(DriverRegistry::default());
        let process_id = std::process::id();
        let driver = registry.driver_for(process_id).unwrap();
        let generation = driver.generation();
        let work_registry = Arc::clone(&registry);

        let observed_generation = driver
            .submit(async move { work_registry.driver_for(process_id).unwrap().generation() })
            .unwrap()
            .wait()
            .unwrap();

        assert_eq!(observed_generation, generation);
    }

    #[test]
    fn stale_driver_refuses_to_submit_to_an_inherited_runtime() {
        let registry = DriverRegistry::default();
        let parent_pid = std::process::id();
        let parent = registry.driver_for(parent_pid).unwrap();

        let result = parent.submit_for_process(async {}, parent_pid.wrapping_add(1));

        assert!(matches!(
            result,
            Err(BlockingDriverError::StaleProcess {
                recorded,
                current,
            }) if recorded == parent_pid && current == parent_pid.wrapping_add(1)
        ));
    }

    #[test]
    fn inherited_submission_try_wait_rejects_the_child_process() {
        let parent_pid = std::process::id();
        let mut submission = pending_submission();

        let result = submission.try_wait_for_process(parent_pid.wrapping_add(1));

        assert_stale_process(result.unwrap_err(), parent_pid);
    }

    #[test]
    fn inherited_submission_wait_rejects_the_child_process_without_blocking() {
        let parent_pid = std::process::id();
        let submission = pending_submission();

        let result = submission.wait_for_process(parent_pid.wrapping_add(1));

        assert_stale_process(result.unwrap_err(), parent_pid);
    }

    #[test]
    fn inherited_submission_cancel_rejects_the_child_process() {
        let parent_pid = std::process::id();
        let submission = pending_submission();

        let result = submission.cancel_for_process(parent_pid.wrapping_add(1));

        assert_stale_process(result.unwrap_err(), parent_pid);
    }

    #[test]
    fn inherited_submission_drop_skips_the_tokio_abort_handle() {
        let parent_pid = std::process::id();
        let mut local_submission = pending_submission();
        let mut inherited_submission = pending_submission();

        assert!(local_submission.finish_drop_for_process(parent_pid));
        assert!(!inherited_submission.finish_drop_for_process(parent_pid.wrapping_add(1)));
    }

    fn pending_submission() -> BlockingSubmission<()> {
        BlockingRuntimeDriver::process_local()
            .unwrap()
            .submit(future::pending())
            .unwrap()
    }

    fn assert_stale_process(error: BlockingTaskError, recorded: u32) {
        assert!(matches!(
            error,
            BlockingTaskError::StaleProcess {
                recorded: error_recorded,
                current,
            } if error_recorded == recorded && current == recorded.wrapping_add(1)
        ));
    }

    #[test]
    fn process_identity_change_leaks_inherited_runtime_and_advances_generation() {
        let registry = DriverRegistry::default();
        let parent_pid = std::process::id();
        let parent = registry.driver_for(parent_pid).unwrap();
        let parent_generation = parent.generation();
        let inherited_runtime = Arc::downgrade(&parent.inner);
        drop(parent);

        let child = registry.driver_for(parent_pid.wrapping_add(1)).unwrap();

        assert_ne!(child.process_id(), parent_pid);
        assert!(child.generation() > parent_generation);
        assert!(
            inherited_runtime.upgrade().is_some(),
            "PID replacement must leak rather than drop/join inherited threads"
        );
    }
}
