use agens_core::IntraTurnInputSource;
use agens_store::{DirectiveGrain, DirectiveStore};

struct Temporary {
    path: std::path::PathBuf,
}

impl Temporary {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("agens-directives-{label}-{}", std::process::id()));
        std::fs::remove_dir_all(&path).ok();
        std::fs::create_dir_all(&path).expect("test data directory");
        Self { path }
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.path).ok();
    }
}

fn store(label: &str) -> (Temporary, DirectiveStore) {
    let temporary = Temporary::new(label);
    let store = DirectiveStore::open(&temporary.path).expect("the queue opens");
    (temporary, store)
}

#[test]
fn a_queued_directive_is_delivered_once_and_in_order() {
    let (_temporary, mut store) = store("order");

    store
        .enqueue(
            7,
            IntraTurnInputSource::Human,
            DirectiveGrain::ToolCall,
            "first",
        )
        .unwrap();
    store
        .enqueue(
            7,
            IntraTurnInputSource::Supervisor,
            DirectiveGrain::ToolCall,
            "second",
        )
        .unwrap();

    let drained = store.drain(7, DirectiveGrain::ToolCall).unwrap();

    assert_eq!(
        drained
            .iter()
            .map(|input| (input.source, input.text.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (IntraTurnInputSource::Human, "first"),
            (IntraTurnInputSource::Supervisor, "second"),
        ]
    );
    assert!(
        store.drain(7, DirectiveGrain::ToolCall).unwrap().is_empty(),
        "a delivered directive is never handed over twice"
    );
}

/// The two grains are separate queues: a directive that must wait for the turn
/// to end is not handed over at a tool boundary just because one came first.
#[test]
fn each_grain_is_drained_on_its_own() {
    let (_temporary, mut store) = store("grain");

    store
        .enqueue(
            1,
            IntraTurnInputSource::Supervisor,
            DirectiveGrain::Turn,
            "replan",
        )
        .unwrap();
    store
        .enqueue(
            1,
            IntraTurnInputSource::Human,
            DirectiveGrain::ToolCall,
            "use the manifest",
        )
        .unwrap();

    let at_tool_call = store.drain(1, DirectiveGrain::ToolCall).unwrap();

    assert_eq!(at_tool_call.len(), 1);
    assert_eq!(at_tool_call[0].text, "use the manifest");
    assert_eq!(store.drain(1, DirectiveGrain::Turn).unwrap().len(), 1);
}

/// One session's queue never reaches another's.
#[test]
fn a_directive_is_scoped_to_its_session() {
    let (_temporary, mut store) = store("scope");

    store
        .enqueue(
            1,
            IntraTurnInputSource::Human,
            DirectiveGrain::ToolCall,
            "for one",
        )
        .unwrap();

    assert!(store.drain(2, DirectiveGrain::ToolCall).unwrap().is_empty());
    assert_eq!(store.drain(1, DirectiveGrain::ToolCall).unwrap().len(), 1);
}

/// A queue that survives the process is the point: a message written while the
/// turn was working must still be there when the turn reaches its boundary.
#[test]
fn a_queued_directive_survives_a_reopen() {
    let temporary = Temporary::new("durable");
    DirectiveStore::open(&temporary.path)
        .unwrap()
        .enqueue(
            3,
            IntraTurnInputSource::Supervisor,
            DirectiveGrain::ToolCall,
            "still here",
        )
        .unwrap();

    let drained = DirectiveStore::open(&temporary.path)
        .unwrap()
        .drain(3, DirectiveGrain::ToolCall)
        .unwrap();

    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].text, "still here");
}

#[test]
fn an_empty_directive_is_refused() {
    let (_temporary, mut store) = store("empty");

    assert!(
        store
            .enqueue(1, IntraTurnInputSource::Human, DirectiveGrain::ToolCall, "")
            .is_err()
    );
}

/// The inbox a turn sees hands over only the tool-call grain. What waits for
/// the turn to end is not a running turn's business: by then it is closed.
#[test]
fn the_turn_facing_inbox_ignores_the_turn_grain() {
    use agens_core::HeadlessIntraTurnInbox;
    use agens_store::DirectiveInbox;

    let temporary = Temporary::new("inbox");
    let mut store = DirectiveStore::open(&temporary.path).unwrap();
    store
        .enqueue(
            5,
            IntraTurnInputSource::Supervisor,
            DirectiveGrain::Turn,
            "replan",
        )
        .unwrap();
    store
        .enqueue(
            5,
            IntraTurnInputSource::Human,
            DirectiveGrain::ToolCall,
            "use the manifest",
        )
        .unwrap();

    let mut inbox = DirectiveInbox::new(store, 5);
    let drained = block_on(inbox.drain()).expect("the inbox reads");

    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].text, "use the manifest");
    assert_eq!(drained[0].source, IntraTurnInputSource::Human);
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};

    let mut future = Box::pin(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
}
