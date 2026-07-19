use std::{
    collections::BTreeMap,
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
pub type HttpWorkerFuture =
    Pin<Box<dyn Future<Output = Result<HttpResponse, HttpWorkerError>> + Send>>;
pub type HttpWorkerCancellationProbe = Arc<dyn Fn() -> bool + Send + Sync>;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub endpoint: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpWorkerError {
    Busy,
    Cancelled,
    TimedOut,
    Startup,
    Panicked,
    Shutdown,
    Transport,
    ResponseTooLarge,
}
pub trait HttpWorkerOperation: Send + 'static {
    fn start(&mut self) -> Result<(), HttpWorkerError>;
    fn execute(&mut self, request: HttpRequest) -> HttpWorkerFuture;
    fn close(&mut self);
}
struct Command {
    request: HttpRequest,
    cancellation: HttpWorkerCancellationProbe,
    deadline: Instant,
    result: SyncSender<Result<HttpResponse, HttpWorkerError>>,
}
struct WorkerState {
    closing: AtomicBool,
    panicked: AtomicBool,
    admitted: AtomicUsize,
}
pub struct HttpWorker {
    commands: SyncSender<Command>,
    state: Arc<WorkerState>,
    join: Mutex<Option<JoinHandle<()>>>,
    capacity: usize,
    #[cfg(test)]
    admission_notice: Mutex<Option<SyncSender<()>>>,
}
impl HttpWorker {
    pub fn start(
        capacity: usize,
        mut operation: impl HttpWorkerOperation,
    ) -> Result<Self, HttpWorkerError> {
        if capacity == 0 {
            return Err(HttpWorkerError::Startup);
        }
        let (commands, receiver) = mpsc::sync_channel(capacity);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let state = Arc::new(WorkerState {
            closing: AtomicBool::new(false),
            panicked: AtomicBool::new(false),
            admitted: AtomicUsize::new(0),
        });
        let worker_state = Arc::clone(&state);
        let join = thread::Builder::new()
            .name("agens-http-worker".into())
            .spawn(move || run_worker(receiver, &mut operation, worker_state, ready_sender))
            .map_err(|_| HttpWorkerError::Startup)?;
        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                commands,
                state,
                join: Mutex::new(Some(join)),
                capacity,
                #[cfg(test)]
                admission_notice: Mutex::new(None),
            }),
            Ok(Err(error)) => {
                let _ = join.join();
                Err(error)
            }
            Err(_) => {
                let _ = join.join();
                Err(HttpWorkerError::Panicked)
            }
        }
    }
    pub fn request(
        &self,
        request: HttpRequest,
        cancellation: HttpWorkerCancellationProbe,
        deadline: Instant,
    ) -> Result<HttpResponse, HttpWorkerError> {
        if self.state.closing.load(Ordering::Acquire) {
            return Err(HttpWorkerError::Shutdown);
        }
        if self
            .state
            .admitted
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < self.capacity).then_some(count + 1)
            })
            .is_err()
        {
            return Err(HttpWorkerError::Busy);
        }
        let (result, response) = mpsc::sync_channel(1);
        let command = Command {
            request,
            cancellation,
            deadline,
            result,
        };
        if self.commands.try_send(command).is_err() {
            self.state.admitted.fetch_sub(1, Ordering::AcqRel);
            return Err(self.disconnected_error());
        }
        #[cfg(test)]
        if let Ok(notice) = self.admission_notice.lock()
            && let Some(notice) = notice.as_ref()
        {
            let _ = notice.send(());
        }
        Self::receive_response(&self.state, response)
    }
    pub fn close(&self) -> Result<(), HttpWorkerError> {
        self.state.closing.store(true, Ordering::Release);
        let join = self
            .join
            .lock()
            .map_err(|_| HttpWorkerError::Panicked)?
            .take();
        if let Some(join) = join {
            join.join().map_err(|_| HttpWorkerError::Panicked)?;
        }
        if self.state.panicked.load(Ordering::Acquire) {
            return Err(HttpWorkerError::Panicked);
        }
        Ok(())
    }
    fn disconnected_error(&self) -> HttpWorkerError {
        if self.state.panicked.load(Ordering::Acquire) {
            HttpWorkerError::Panicked
        } else {
            HttpWorkerError::Shutdown
        }
    }
    fn receive_response(
        state: &WorkerState,
        response: Receiver<Result<HttpResponse, HttpWorkerError>>,
    ) -> Result<HttpResponse, HttpWorkerError> {
        response.recv().unwrap_or_else(|_| {
            if state.panicked.load(Ordering::Acquire) {
                Err(HttpWorkerError::Panicked)
            } else {
                Err(HttpWorkerError::Shutdown)
            }
        })
    }
    #[cfg(test)]
    fn set_admission_notice_for_test(&self, notice: SyncSender<()>) {
        *self.admission_notice.lock().unwrap() = Some(notice);
    }
}
impl Drop for HttpWorker {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
fn run_worker(
    receiver: Receiver<Command>,
    operation: &mut impl HttpWorkerOperation,
    state: Arc<WorkerState>,
    ready: SyncSender<Result<(), HttpWorkerError>>,
) {
    let startup = catch_unwind(AssertUnwindSafe(|| {
        operation.start()?;
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .map_err(|_| HttpWorkerError::Startup)
    }));
    let runtime = match startup {
        Ok(Ok(runtime)) => {
            let _ = ready.send(Ok(()));
            runtime
        }
        Ok(Err(error)) => {
            let _ = ready.send(Err(error));
            operation.close();
            return;
        }
        Err(_) => {
            state.panicked.store(true, Ordering::Release);
            let _ = ready.send(Err(HttpWorkerError::Panicked));
            operation.close();
            return;
        }
    };
    while !state.closing.load(Ordering::Acquire) {
        let Ok(command) = receiver.recv_timeout(Duration::from_millis(1)) else {
            continue;
        };
        let Command {
            request,
            cancellation,
            deadline,
            result: response,
        } = command;
        let result = catch_unwind(AssertUnwindSafe(|| {
            runtime.block_on(run_request(
                operation.execute(request),
                &cancellation,
                deadline,
                &state,
            ))
        }))
        .unwrap_or_else(|_| {
            state.panicked.store(true, Ordering::Release);
            Err(HttpWorkerError::Panicked)
        });
        let _ = response.send(result);
        state.admitted.fetch_sub(1, Ordering::AcqRel);
        if state.panicked.load(Ordering::Acquire) {
            break;
        }
    }
    for command in receiver.try_iter() {
        let _ = command.result.send(Err(HttpWorkerError::Shutdown));
        state.admitted.fetch_sub(1, Ordering::AcqRel);
    }
    operation.close();
}
async fn run_request(
    future: HttpWorkerFuture,
    cancellation: &HttpWorkerCancellationProbe,
    deadline: Instant,
    state: &WorkerState,
) -> Result<HttpResponse, HttpWorkerError> {
    let mut ticker = tokio::time::interval(Duration::from_millis(1));
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return result,
            _ = ticker.tick() => {
                if cancellation() {
                    return Err(HttpWorkerError::Cancelled);
                }
                if Instant::now() >= deadline {
                    return Err(HttpWorkerError::TimedOut);
                }
                if state.closing.load(Ordering::Acquire) {
                    return Err(HttpWorkerError::Shutdown);
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    enum Behavior {
        Pending,
        Respond,
        Panic,
        StartupError,
        StartupPanic,
    }
    struct Operation {
        behavior: Behavior,
        started: Option<SyncSender<()>>,
        closes: Arc<AtomicUsize>,
    }
    impl HttpWorkerOperation for Operation {
        fn start(&mut self) -> Result<(), HttpWorkerError> {
            match self.behavior {
                Behavior::StartupError => Err(HttpWorkerError::Startup),
                Behavior::StartupPanic => panic!("test startup panic"),
                _ => Ok(()),
            }
        }
        fn execute(&mut self, _: HttpRequest) -> HttpWorkerFuture {
            if let Some(started) = self.started.take() {
                started.send(()).unwrap();
            }
            match self.behavior {
                Behavior::Pending => Box::pin(std::future::pending()),
                Behavior::Respond => Box::pin(std::future::ready(Ok(response()))),
                Behavior::Panic => panic!("test worker panic"),
                Behavior::StartupError | Behavior::StartupPanic => unreachable!(),
            }
        }
        fn close(&mut self) {
            self.closes.fetch_add(1, Ordering::AcqRel);
        }
    }
    fn worker(
        capacity: usize,
        behavior: Behavior,
        started: Option<SyncSender<()>>,
    ) -> (HttpWorker, Arc<AtomicUsize>) {
        let closes = Arc::new(AtomicUsize::new(0));
        let operation = Operation {
            behavior,
            started,
            closes: Arc::clone(&closes),
        };
        (HttpWorker::start(capacity, operation).unwrap(), closes)
    }
    fn request() -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            endpoint: "https://example.invalid/mcp".into(),
            headers: BTreeMap::new(),
            body: Vec::new(),
        }
    }
    fn response() -> HttpResponse {
        HttpResponse {
            status: 200,
            body: b"ok".to_vec(),
        }
    }
    fn context(timeout: Duration) -> (Arc<AtomicBool>, Instant) {
        (Arc::new(AtomicBool::new(false)), Instant::now() + timeout)
    }
    #[test]
    fn bounds_admission_and_aborts_for_cancellation_and_deadline() {
        for (timeout, expected, cancel) in [
            (Duration::from_secs(1), HttpWorkerError::Cancelled, true),
            (Duration::from_millis(10), HttpWorkerError::TimedOut, false),
        ] {
            let (started_sender, started_receiver) = mpsc::sync_channel(1);
            let (worker, _) = worker(1, Behavior::Pending, Some(started_sender));
            let worker = Arc::new(worker);
            let (cancellation, deadline) = context(timeout);
            let active_worker = Arc::clone(&worker);
            let active_cancellation = Arc::clone(&cancellation);
            let active = thread::spawn(move || {
                active_worker.request(
                    request(),
                    Arc::new(move || active_cancellation.load(Ordering::Acquire)),
                    deadline,
                )
            });
            started_receiver
                .recv_timeout(Duration::from_millis(250))
                .unwrap();
            let (other_cancellation, other_deadline) = context(Duration::from_secs(1));
            assert_eq!(
                worker.request(
                    request(),
                    Arc::new(move || other_cancellation.load(Ordering::Acquire)),
                    other_deadline
                ),
                Err(HttpWorkerError::Busy)
            );
            if cancel {
                cancellation.store(true, Ordering::Release);
            }
            assert_eq!(active.join().unwrap(), Err(expected));
            worker.close().unwrap();
        }
    }
    #[test]
    fn startup_error_returns_startup() {
        let closes = Arc::new(AtomicUsize::new(0));
        let result = HttpWorker::start(
            1,
            Operation {
                behavior: Behavior::StartupError,
                started: None,
                closes: Arc::clone(&closes),
            },
        );

        assert!(matches!(result, Err(HttpWorkerError::Startup)));
        assert_eq!(closes.load(Ordering::Acquire), 1);
    }

    #[test]
    fn startup_panic_returns_panicked() {
        let closes = Arc::new(AtomicUsize::new(0));
        let result = HttpWorker::start(
            1,
            Operation {
                behavior: Behavior::StartupPanic,
                started: None,
                closes: Arc::clone(&closes),
            },
        );

        assert!(matches!(result, Err(HttpWorkerError::Panicked)));
        assert_eq!(closes.load(Ordering::Acquire), 1);
    }

    #[test]
    fn zero_capacity_startup_returns_startup() {
        let closes = Arc::new(AtomicUsize::new(0));
        let result = HttpWorker::start(
            0,
            Operation {
                behavior: Behavior::Respond,
                started: None,
                closes: Arc::clone(&closes),
            },
        );

        assert!(matches!(result, Err(HttpWorkerError::Startup)));
        assert_eq!(closes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn operation_panic_returns_panicked() {
        let (worker, _) = worker(1, Behavior::Panic, None);
        let (cancellation, deadline) = context(Duration::from_secs(1));
        assert_eq!(
            worker.request(
                request(),
                Arc::new(move || cancellation.load(Ordering::Acquire)),
                deadline
            ),
            Err(HttpWorkerError::Panicked)
        );
    }
    #[test]
    fn close_drop_and_current_thread_call_are_safe() {
        let (response_worker, closes) = worker(1, Behavior::Respond, None);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let result = runtime.block_on(async {
            let (cancellation, deadline) = context(Duration::from_secs(1));
            response_worker.request(
                request(),
                Arc::new(move || cancellation.load(Ordering::Acquire)),
                deadline,
            )
        });
        assert_eq!(result, Ok(response()));
        response_worker.close().unwrap();
        response_worker.close().unwrap();
        assert_eq!(closes.load(Ordering::Acquire), 1);
        let (dropped_worker, dropped) = worker(1, Behavior::Respond, None);
        drop(dropped_worker);
        assert_eq!(dropped.load(Ordering::Acquire), 1);
    }

    #[test]
    fn shutdown_rejects_active_queued_and_post_close_requests() {
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let (worker, closes) = worker(2, Behavior::Pending, Some(started_sender));
        let worker = Arc::new(worker);

        let (cancellation, deadline) = context(Duration::from_secs(1));
        let active_worker = Arc::clone(&worker);
        let active = thread::spawn(move || {
            active_worker.request(
                request(),
                Arc::new(move || cancellation.load(Ordering::Acquire)),
                deadline,
            )
        });
        started_receiver
            .recv_timeout(Duration::from_millis(250))
            .unwrap();

        let (queued_sender, queued_receiver) = mpsc::sync_channel(1);
        worker.set_admission_notice_for_test(queued_sender);
        let queued_worker = Arc::clone(&worker);
        let queued = thread::spawn(move || {
            let (cancellation, deadline) = context(Duration::from_secs(1));
            queued_worker.request(
                request(),
                Arc::new(move || cancellation.load(Ordering::Acquire)),
                deadline,
            )
        });
        queued_receiver
            .recv_timeout(Duration::from_millis(250))
            .unwrap();

        worker.close().unwrap();
        assert_eq!(active.join().unwrap(), Err(HttpWorkerError::Shutdown));
        assert_eq!(queued.join().unwrap(), Err(HttpWorkerError::Shutdown));
        let (cancellation, deadline) = context(Duration::from_secs(1));
        assert_eq!(
            worker.request(
                request(),
                Arc::new(move || cancellation.load(Ordering::Acquire)),
                deadline
            ),
            Err(HttpWorkerError::Shutdown)
        );
        assert_eq!(closes.load(Ordering::Acquire), 1);
    }

    #[test]
    fn disconnected_result_channel_maps_to_shutdown() {
        let (worker, _) = worker(1, Behavior::Respond, None);
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(sender);

        assert_eq!(
            HttpWorker::receive_response(&worker.state, receiver),
            Err(HttpWorkerError::Shutdown)
        );
        worker.close().unwrap();
    }
}
