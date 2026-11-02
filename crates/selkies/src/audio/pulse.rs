/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The libpulse control connection on a dedicated thread.
//!
//! A [`PulseBackend`] owns one thread that holds a standard mainloop and a context of its own,
//! since neither may leave the thread that created them, and serves requests from a queue one at
//! a time: it issues the operation, pumps the mainloop in bounded steps until the operation's
//! callback answers or the request's deadline passes, and sends the result back over a oneshot.
//! The caller side is plain async: a request is queued, its answer awaited, and nothing of
//! libpulse is touched off the thread.
//!
//! The never-cancel discipline: a pending operation is never canceled. A deadline abandons the
//! connection instead (`disconnect` drops the pending operations' callbacks on the server side)
//! and the request answers `Timeout`; a caller whose future is dropped leaves the thread to
//! finish the operation on its own, and the connection stays usable for the next request. Only a
//! `Close` request, queued behind the in-flight operations, ends the thread, and dropping the
//! backend does the same once the queue drains.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use libpulse_binding as pulse;
use log::debug;
use pulse::callbacks::ListResult;
use pulse::context::introspect::{ServerInfo, SinkInfo, SourceInfo, SourceOutputInfo};
use pulse::context::{Context, FlagSet as CtxFlags, State};
use pulse::def::INVALID_INDEX;
use pulse::mainloop::standard::Mainloop;
use pulse::proplist::Proplist;
use pulse::time::MicroSeconds;
use tokio::sync::oneshot;

use super::control::{AudioControlError, ControlBackend, PulseNode};

/// One mainloop step: how long the thread blocks for events before it re-checks its deadline.
const PUMP_US: u64 = 20 * 1000;
/// How long the idle thread waits for a request before pumping the mainloop once, so a server
/// that went away is noticed by the connection state rather than by the next operation alone.
const IDLE_POLL: Duration = Duration::from_millis(500);

/// A server call the thread issues.
enum Op {
    SinkList,
    SourceList,
    SourceOutputList,
    ServerInfo,
    LoadModule(String, String),
    UnloadModule(u32),
    SetDefaultSink(String),
    SetDefaultSource(String),
    MoveSourceOutput(u32, u32),
}

impl Op {
    fn name(&self) -> &'static str {
        match self {
            Op::SinkList => "sink list",
            Op::SourceList => "source list",
            Op::SourceOutputList => "source-output list",
            Op::ServerInfo => "server info",
            Op::LoadModule(..) => "load-module",
            Op::UnloadModule(_) => "unload-module",
            Op::SetDefaultSink(_) => "set-default-sink",
            Op::SetDefaultSource(_) => "set-default-source",
            Op::MoveSourceOutput(..) => "move-source-output",
        }
    }
}

/// What a call answered.
enum Reply {
    Nodes(Vec<PulseNode>),
    Defaults(Option<String>, Option<String>),
    Index(u32),
    Done,
}

type Answer = Result<Reply, AudioControlError>;

enum Request {
    Op {
        op: Op,
        deadline: Instant,
        reply: oneshot::Sender<Answer>,
    },
    Close {
        done: oneshot::Sender<()>,
    },
}

/// The connection thread's handle: the request queue and the connection's last known state.
pub struct PulseBackend {
    tx: mpsc::Sender<Request>,
    connected: Arc<AtomicBool>,
    op_timeout: Duration,
}

impl PulseBackend {
    /// Connect as `client_name`, waiting at most `connect_timeout` for the handshake; the thread
    /// serves requests afterwards, each bounded by `op_timeout`. The error is the reason the
    /// server could not be reached.
    pub async fn connect(
        client_name: String,
        connect_timeout: Duration,
        op_timeout: Duration,
    ) -> Result<PulseBackend, String> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let connected = Arc::new(AtomicBool::new(false));
        let flag = connected.clone();
        thread::Builder::new()
            .name("audio-control".to_string())
            .spawn(move || serve(client_name, connect_timeout, ready_tx, rx, flag))
            .map_err(|e| format!("control thread spawn failed: {e}"))?;
        match ready_rx.await {
            Ok(Ok(())) => Ok(PulseBackend {
                tx,
                connected,
                op_timeout,
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("control thread ended before connecting".to_string()),
        }
    }

    /// Queue `op` and await its answer. The thread enforces the deadline; the wait here is only
    /// bounded again as a safety net against a thread that stopped answering.
    async fn call(&self, op: Op) -> Answer {
        let (reply, rx) = oneshot::channel();
        let deadline = Instant::now() + self.op_timeout;
        self.tx
            .send(Request::Op {
                op,
                deadline,
                reply,
            })
            .map_err(|_| AudioControlError::NotConnected)?;
        let net = self.op_timeout * 2 + Duration::from_secs(1);
        match tokio::time::timeout(net, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => {
                self.connected.store(false, Ordering::Release);
                Err(AudioControlError::NotConnected)
            }
            Err(_) => {
                self.connected.store(false, Ordering::Release);
                Err(AudioControlError::Timeout)
            }
        }
    }

    async fn nodes(&self, op: Op) -> Result<Vec<PulseNode>, AudioControlError> {
        match self.call(op).await? {
            Reply::Nodes(nodes) => Ok(nodes),
            _ => Err(AudioControlError::Failed("unexpected answer".to_string())),
        }
    }

    async fn done(&self, op: Op) -> Result<(), AudioControlError> {
        self.call(op).await.map(drop)
    }
}

#[async_trait]
impl ControlBackend for PulseBackend {
    fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    async fn sink_list(&mut self) -> Result<Vec<PulseNode>, AudioControlError> {
        self.nodes(Op::SinkList).await
    }

    async fn source_list(&mut self) -> Result<Vec<PulseNode>, AudioControlError> {
        self.nodes(Op::SourceList).await
    }

    async fn source_output_list(&mut self) -> Result<Vec<PulseNode>, AudioControlError> {
        self.nodes(Op::SourceOutputList).await
    }

    async fn server_defaults(
        &mut self,
    ) -> Result<(Option<String>, Option<String>), AudioControlError> {
        match self.call(Op::ServerInfo).await? {
            Reply::Defaults(sink, source) => Ok((sink, source)),
            _ => Err(AudioControlError::Failed("unexpected answer".to_string())),
        }
    }

    async fn module_load(&mut self, name: &str, args: &str) -> Result<u32, AudioControlError> {
        match self
            .call(Op::LoadModule(name.to_string(), args.to_string()))
            .await?
        {
            Reply::Index(index) => Ok(index),
            _ => Err(AudioControlError::Failed("unexpected answer".to_string())),
        }
    }

    async fn module_unload(&mut self, index: u32) -> Result<(), AudioControlError> {
        self.done(Op::UnloadModule(index)).await
    }

    async fn sink_default_set(&mut self, name: &str) -> Result<(), AudioControlError> {
        self.done(Op::SetDefaultSink(name.to_string())).await
    }

    async fn source_default_set(&mut self, name: &str) -> Result<(), AudioControlError> {
        self.done(Op::SetDefaultSource(name.to_string())).await
    }

    async fn source_output_move(
        &mut self,
        output_index: u32,
        source_index: u32,
    ) -> Result<(), AudioControlError> {
        self.done(Op::MoveSourceOutput(output_index, source_index))
            .await
    }

    async fn close(&mut self) {
        let (done, rx) = oneshot::channel();
        if self.tx.send(Request::Close { done }).is_ok() {
            let _ = rx.await;
        }
        self.connected.store(false, Ordering::Release);
    }
}

/// One bounded mainloop iteration (`prepare` → `poll` → `dispatch`); false when a stage failed.
fn pump(ml: &mut Mainloop, timeout_us: u64) -> bool {
    ml.prepare(Some(MicroSeconds(timeout_us))).is_ok() && ml.poll().is_ok() && ml.dispatch().is_ok()
}

/// Connect a fresh mainloop and context, pumping until the context is ready or the deadline.
fn open(client_name: &str, connect_timeout: Duration) -> Result<(Mainloop, Context), String> {
    let mut ml = Mainloop::new().ok_or("pa_mainloop_new() failed")?;
    let mut ctx = Context::new(&ml, client_name).ok_or("pa_context_new() failed")?;
    ctx.connect(None, CtxFlags::NOFLAGS, None)
        .map_err(|e| format!("pa_context_connect() failed: {e}"))?;
    let deadline = Instant::now() + connect_timeout;
    loop {
        match ctx.get_state() {
            State::Ready => return Ok((ml, ctx)),
            State::Failed | State::Terminated => {
                return Err(format!("connection failed: {}", ctx.errno()));
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            ctx.disconnect();
            return Err("connection timed out".to_string());
        }
        if !pump(&mut ml, PUMP_US) {
            return Err("mainloop iteration failed during connect".to_string());
        }
    }
}

/// The thread body: connect, report, then serve requests until closed or dropped.
fn serve(
    client_name: String,
    connect_timeout: Duration,
    ready: oneshot::Sender<Result<(), String>>,
    rx: mpsc::Receiver<Request>,
    connected: Arc<AtomicBool>,
) {
    let (mut ml, mut ctx) = match open(&client_name, connect_timeout) {
        Ok(session) => session,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    connected.store(true, Ordering::Release);
    if ready.send(Ok(())).is_err() {
        ctx.disconnect();
        return;
    }
    loop {
        match rx.recv_timeout(IDLE_POLL) {
            Ok(Request::Op {
                op,
                deadline,
                reply,
            }) => {
                let answer = run_op(&mut ml, &mut ctx, op, deadline);
                connected.store(ctx.get_state() == State::Ready, Ordering::Release);
                let _ = reply.send(answer);
            }
            Ok(Request::Close { done }) => {
                ctx.disconnect();
                connected.store(false, Ordering::Release);
                let _ = done.send(());
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                pump(&mut ml, 0);
                connected.store(ctx.get_state() == State::Ready, Ordering::Release);
            }
            Err(RecvTimeoutError::Disconnected) => {
                ctx.disconnect();
                connected.store(false, Ordering::Release);
                return;
            }
        }
    }
}

/// The slot an operation's callback fills.
type Slot = Rc<RefCell<Option<Answer>>>;

fn settle(slot: &Slot, answer: Answer) {
    *slot.borrow_mut() = Some(answer);
}

fn proplist_map(proplist: &Proplist) -> HashMap<String, String> {
    proplist
        .iter()
        .filter_map(|key| proplist.get_str(&key).map(|value| (key, value)))
        .collect()
}

fn sink_node(info: &SinkInfo) -> PulseNode {
    PulseNode {
        index: info.index,
        name: info.name.as_deref().unwrap_or("").to_string(),
        owner_module: info.owner_module,
        proplist: proplist_map(&info.proplist),
        source: None,
        corked: false,
    }
}

fn source_node(info: &SourceInfo) -> PulseNode {
    PulseNode {
        index: info.index,
        name: info.name.as_deref().unwrap_or("").to_string(),
        owner_module: info.owner_module,
        proplist: proplist_map(&info.proplist),
        source: None,
        corked: false,
    }
}

fn source_output_node(info: &SourceOutputInfo) -> PulseNode {
    PulseNode {
        index: info.index,
        name: info.name.as_deref().unwrap_or("").to_string(),
        owner_module: info.owner_module,
        proplist: proplist_map(&info.proplist),
        source: Some(info.source),
        corked: info.corked,
    }
}

/// A list callback accumulating nodes into the slot at `End`; a macro, since each info type
/// carries its own lifetime the binding's callback bound ranges over.
macro_rules! list_callback {
    ($slot:expr, $what:expr, $convert:expr) => {{
        let slot = $slot.clone();
        let what = $what;
        let acc = Rc::new(RefCell::new(Vec::new()));
        move |result| match result {
            ListResult::Item(item) => acc.borrow_mut().push($convert(item)),
            ListResult::End => settle(
                &slot,
                Ok(Reply::Nodes(std::mem::take(&mut *acc.borrow_mut()))),
            ),
            ListResult::Error => settle(
                &slot,
                Err(AudioControlError::Failed(format!("{what} failed"))),
            ),
        }
    }};
}

/// A success callback settling `Done` or the failure named `what`.
fn success_callback(slot: &Slot, what: &'static str) -> impl FnMut(bool) + 'static {
    let slot = slot.clone();
    move |ok| {
        let answer = if ok {
            Ok(Reply::Done)
        } else {
            Err(AudioControlError::Failed(format!("{what} failed")))
        };
        settle(&slot, answer);
    }
}

/// Issue `op` on the thread's context and pump until it answers or `deadline` passes.
///
/// The names an operation carries are refused with an interior NUL rather than handed to the
/// binding, which would panic building the C string; a context no longer ready answers
/// `NotConnected` so the control reconnects. The operation handle lives until the answer; on a
/// deadline the connection is abandoned first, so the server's late reply finds no callback.
fn run_op(ml: &mut Mainloop, ctx: &mut Context, op: Op, deadline: Instant) -> Answer {
    if ctx.get_state() != State::Ready {
        return Err(AudioControlError::NotConnected);
    }
    let what = op.name();
    let slot: Slot = Rc::new(RefCell::new(None));
    let has_nul = |text: &str| text.contains('\0');
    let _operation: Box<dyn std::any::Any> = match op {
        Op::SinkList => Box::new(
            ctx.introspect()
                .get_sink_info_list(list_callback!(slot, what, sink_node)),
        ),
        Op::SourceList => Box::new(ctx.introspect().get_source_info_list(list_callback!(
            slot,
            what,
            source_node
        ))),
        Op::SourceOutputList => {
            Box::new(ctx.introspect().get_source_output_info_list(list_callback!(
                slot,
                what,
                source_output_node
            )))
        }
        Op::ServerInfo => {
            let slot = slot.clone();
            Box::new(ctx.introspect().get_server_info(move |info: &ServerInfo| {
                let name = |v: &Option<std::borrow::Cow<'_, str>>| {
                    v.as_deref().filter(|s| !s.is_empty()).map(str::to_string)
                };
                settle(
                    &slot,
                    Ok(Reply::Defaults(
                        name(&info.default_sink_name),
                        name(&info.default_source_name),
                    )),
                );
            }))
        }
        Op::LoadModule(name, args) => {
            if has_nul(&name) || has_nul(&args) {
                return Err(AudioControlError::Failed(
                    "module arguments contain NUL".to_string(),
                ));
            }
            let slot = slot.clone();
            Box::new(ctx.introspect().load_module(&name, &args, move |index| {
                let answer = if index == INVALID_INDEX {
                    Err(AudioControlError::Failed("load-module failed".to_string()))
                } else {
                    Ok(Reply::Index(index))
                };
                settle(&slot, answer);
            }))
        }
        Op::UnloadModule(index) => Box::new(
            ctx.introspect()
                .unload_module(index, success_callback(&slot, what)),
        ),
        Op::SetDefaultSink(name) => {
            if has_nul(&name) {
                return Err(AudioControlError::Failed(
                    "sink name contains NUL".to_string(),
                ));
            }
            Box::new(ctx.set_default_sink(&name, success_callback(&slot, what)))
        }
        Op::SetDefaultSource(name) => {
            if has_nul(&name) {
                return Err(AudioControlError::Failed(
                    "source name contains NUL".to_string(),
                ));
            }
            Box::new(ctx.set_default_source(&name, success_callback(&slot, what)))
        }
        Op::MoveSourceOutput(output, source) => {
            Box::new(ctx.introspect().move_source_output_by_index(
                output,
                source,
                Some(Box::new(success_callback(&slot, what))),
            ))
        }
    };
    loop {
        if let Some(answer) = slot.borrow_mut().take() {
            return answer;
        }
        if ctx.get_state() != State::Ready {
            return Err(AudioControlError::NotConnected);
        }
        if Instant::now() >= deadline {
            debug!(target: "audio", "Sound server {what} passed its deadline; abandoning the connection.");
            ctx.disconnect();
            return Err(AudioControlError::Timeout);
        }
        if !pump(ml, PUMP_US) {
            return Err(AudioControlError::Failed(
                "mainloop iteration failed".to_string(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    //! Against a real sound server, which `PULSE_SERVER` names.

    use super::*;

    fn server_reachable() -> bool {
        std::env::var_os("PULSE_SERVER").is_some() || std::env::var_os("XDG_RUNTIME_DIR").is_some()
    }

    #[tokio::test]
    async fn lists_and_defaults_answer_over_the_thread() {
        if !server_reachable() {
            panic!("a sound server is required: set PULSE_SERVER");
        }
        let mut backend = PulseBackend::connect(
            "selkies-test-control".to_string(),
            Duration::from_secs(2),
            Duration::from_secs(5),
        )
        .await
        .expect("connect to the sandbox sound server");
        assert!(backend.connected());
        let sinks = backend.sink_list().await.expect("sink list");
        let sources = backend.source_list().await.expect("source list");
        assert!(sources.iter().all(|s| !s.name.is_empty()));
        let (default_sink, _) = backend.server_defaults().await.expect("server info");
        if let Some(default_sink) = default_sink {
            assert!(
                sinks.iter().any(|s| s.name == default_sink),
                "default sink is listed"
            );
        }
        backend
            .source_output_list()
            .await
            .expect("source-output list");
        backend.close().await;
        assert!(!backend.connected());
        assert_eq!(
            backend.sink_list().await,
            Err(AudioControlError::NotConnected)
        );
    }

    #[tokio::test]
    async fn a_module_loads_and_unloads() {
        if !server_reachable() {
            panic!("a sound server is required: set PULSE_SERVER");
        }
        let mut backend = PulseBackend::connect(
            "selkies-test-module".to_string(),
            Duration::from_secs(2),
            Duration::from_secs(5),
        )
        .await
        .expect("connect");
        let name = format!("selkies-test-{}", std::process::id());
        let index = backend
            .module_load("module-null-sink", &format!("sink_name={name}"))
            .await
            .expect("load module-null-sink");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut seen = false;
        while Instant::now() < deadline {
            if backend
                .sink_list()
                .await
                .unwrap()
                .iter()
                .any(|s| s.name == name)
            {
                seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(seen, "the loaded sink appeared");
        // The never-cancel discipline: the unload is queued behind another load and its caller
        // drops it before the thread reaches it; the thread still runs it to completion on the
        // connection it keeps.
        let mut other = Box::pin(backend.call(Op::LoadModule(
            "module-null-sink".to_string(),
            format!("sink_name={name}-b"),
        )));
        assert!(
            futures::poll!(&mut other).is_pending(),
            "the load is queued"
        );
        let mut unload = Box::pin(backend.call(Op::UnloadModule(index)));
        assert!(
            futures::poll!(&mut unload).is_pending(),
            "the unload is queued behind it"
        );
        drop(unload);
        let other_index = match other.await {
            Ok(Reply::Index(other_index)) => other_index,
            other => panic!("the queued load answers: {:?}", other.map(|_| ())),
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut gone = false;
        while Instant::now() < deadline {
            if !backend
                .sink_list()
                .await
                .unwrap()
                .iter()
                .any(|s| s.name == name)
            {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(gone, "the abandoned operation finished on its own");
        assert!(
            backend.connected(),
            "the connection is kept and still usable"
        );
        backend
            .module_unload(other_index)
            .await
            .expect("unload the second sink");
        backend.close().await;
    }
}
