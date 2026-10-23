//! Logon-authenticated, bounded local invitation handoff. No profile/media init.
use crate::{
    instance_identity::{self, Election, Identity, Lease},
    invitation::Invitation,
    invitation_launch::Offer,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    io,
    os::windows::io::{AsRawHandle, IntoRawHandle},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions},
    sync::oneshot,
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, HANDLE},
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_MODE, OPEN_EXISTING,
            SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
        },
        UI::WindowsAndMessaging::AllowSetForegroundWindow,
    },
};

const INVALID: &str =
    "Relay could not verify the open invitation. Open Relay and paste it in Join friends.";
const BUSY: &str="Relay is busy starting, updating or using another invitation. Check its open window, then try again.";
const MAX_FRAME: usize = 4352;
const EXCHANGE: Duration = Duration::from_secs(2);

// An invitation now also carries its extra routes; the value is short-lived.
#[allow(clippy::large_enum_variant)]
pub enum Action {
    Activate,
    Open(Invitation),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u8,
    request: String,
    invitation: serde_json::Value,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Status {
    Accepted,
    Busy,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    version: u8,
    request: String,
    status: Status,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u8,
    request: String,
    received: bool,
}

fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn action(request: &Request) -> Result<Action, &'static str> {
    if request.version != 1 || !valid_id(&request.request) {
        return Err(INVALID);
    }
    match &request.invitation {
        serde_json::Value::Null => Ok(Action::Activate),
        serde_json::Value::String(text)
            if text.len() <= 4096 && !text.chars().any(char::is_control) =>
        {
            let invite = Invitation::parse(text).map_err(|_| INVALID)?;
            if invite.link() != *text {
                return Err(INVALID);
            }
            Ok(Action::Open(invite))
        }
        _ => Err(INVALID),
    }
}

async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Vec<u8>> {
    let length = reader.read_u32_le().await? as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), body: &[u8]) -> io::Result<()> {
    if body.is_empty() || body.len() > MAX_FRAME {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    writer.write_u32_le(body.len() as u32).await?;
    writer.write_all(body).await?;
    writer.flush().await
}

pub struct Incoming {
    pub action: Action,
    reply: oneshot::Sender<Status>,
    deadline: Instant,
}
impl Incoming {
    pub fn current(&self) -> bool {
        Instant::now() < self.deadline && !self.reply.is_closed()
    }
    pub fn respond(self, offer: Offer) {
        let _ = self.reply.send(if offer == Offer::Accepted {
            Status::Accepted
        } else {
            Status::Busy
        });
    }
}
type Callback = Arc<dyn Fn() + Send + Sync>;
#[derive(Default)]
struct WakeState {
    callback: Option<Callback>,
    pending: bool,
}
type Wake = Arc<Mutex<WakeState>>;
pub struct Inbox {
    receiver: mpsc::Receiver<Incoming>,
    wake: Wake,
}
impl Inbox {
    pub fn attach(&self, wake: impl Fn() + Send + Sync + 'static) {
        let notify = if let Ok(mut current) = self.wake.lock() {
            current.callback = Some(Arc::new(wake));
            current.pending.then(|| current.callback.clone()).flatten()
        } else {
            None
        };
        // Never call UI/user code while holding the queue's coordination lock.
        if let Some(notify) = notify {
            notify();
        }
    }
    pub fn next(&self) -> Option<Incoming> {
        let mut current = self.wake.lock().ok()?;
        let incoming = self.receiver.try_recv().ok()?;
        current.pending = false;
        Some(incoming)
    }
}

pub struct Owner {
    _lease: Lease,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // The lifetime mutex is released only after all pipe I/O has stopped.
    }
}
pub enum Start {
    Owner(Owner, Inbox),
    Forwarded,
}

fn runtime() -> Result<tokio::runtime::Runtime, &'static str> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|_| INVALID)
}

pub fn start(invitation: Option<Invitation>) -> Result<Start, &'static str> {
    start_scoped("main", invitation, true)
}

/// Keep a browser's capability-bearing argv out of the long-lived UI process.
/// Cold and warm opens both use the same authenticated UI acknowledgement.
pub fn open(invitation: Invitation) -> Result<(), &'static str> {
    open_scoped("main", invitation, true, || {
        use std::{
            os::windows::process::CommandExt,
            process::{Command, Stdio},
        };
        let executable = std::env::current_exe().map_err(|_| INVALID)?;
        Command::new(executable)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x08000000)
            .spawn()
            .map_err(|_| INVALID)?;
        Ok(())
    })
}

fn open_scoped(
    scope: &str,
    invitation: Invitation,
    focus: bool,
    launch: impl FnOnce() -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    let identity = Identity::current()?;
    let namespace = identity.namespace(scope);
    match instance_identity::elect(&identity, &namespace)? {
        Election::Existing => {}
        Election::Owner(lease) => {
            // Release before starting the clean main. Concurrent clean launches
            // still elect exactly one owner before any profile/media writer.
            drop(lease);
            launch()?;
        }
    }
    forward(
        &identity,
        &format!(r"\\.\pipe\{namespace}"),
        Some(invitation),
        focus,
    )
}

fn start_scoped(
    scope: &str,
    invitation: Option<Invitation>,
    focus: bool,
) -> Result<Start, &'static str> {
    let identity = Identity::current()?;
    let namespace = identity.namespace(scope);
    let pipe = format!(r"\\.\pipe\{namespace}");
    match instance_identity::elect(&identity, &namespace)? {
        Election::Existing => {
            forward(&identity, &pipe, invitation, focus)?;
            Ok(Start::Forwarded)
        }
        Election::Owner(lease) => {
            let (sender, receiver) = mpsc::sync_channel(1);
            let wake: Wake = Arc::new(Mutex::new(WakeState::default()));
            let (stop, stop_rx) = oneshot::channel();
            let (ready, ready_rx) = mpsc::sync_channel(1);
            let worker_wake = wake.clone();
            let worker = thread::Builder::new()
                .name("relay-invitation".into())
                .spawn(move || {
                    let result = runtime();
                    let Ok(runtime) = result else {
                        let _ = ready.send(Err(INVALID));
                        return;
                    };
                    runtime.block_on(async move {
                        let server = server(&identity, &pipe);
                        let Ok(server) = server else {
                            let _ = ready.send(Err(INVALID));
                            return;
                        };
                        if ready.send(Ok(())).is_err() {
                            return;
                        }
                        serve(server, identity, pipe, sender, worker_wake, stop_rx).await;
                    });
                })
                .map_err(|_| INVALID)?;
            let owner = Owner {
                _lease: lease,
                stop: Some(stop),
                worker: Some(worker),
            };
            match ready_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(())) => Ok(Start::Owner(owner, Inbox { receiver, wake })),
                _ => Err(INVALID),
            }
        }
    }
}

fn server(identity: &Identity, name: &str) -> Result<NamedPipeServer, &'static str> {
    let descriptor = identity.descriptor(true)?;
    let mut attributes = descriptor.attributes();
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .max_instances(1)
            .in_buffer_size(MAX_FRAME as u32)
            .out_buffer_size(MAX_FRAME as u32)
            .create_with_security_attributes_raw(
                name,
                (&mut attributes as *mut windows::Win32::Security::SECURITY_ATTRIBUTES).cast(),
            )
    }
    .map_err(|_| INVALID)
}

async fn serve(
    mut pipe: NamedPipeServer,
    identity: Identity,
    name: String,
    sender: mpsc::SyncSender<Incoming>,
    wake: Wake,
    mut stop: oneshot::Receiver<()>,
) {
    let mut accepted: VecDeque<(String, [u8; 32])> = VecDeque::new();
    loop {
        tokio::select! {
            biased;
            _=&mut stop=>break,
            result=pipe.connect()=>{if result.is_err(){break}}
        }
        let verified = identity.peer(HANDLE(pipe.as_raw_handle()), false);
        if let Ok(_peer) = verified {
            tokio::select! {
                biased;
                _=&mut stop=>break,
                _=tokio::time::timeout(EXCHANGE,exchange(&mut pipe,&sender,&wake,&mut accepted))=>{}
            }
        }
        if pipe.disconnect().is_err() {
            break;
        }
        // Mio may retain a completed read/EOF for a disconnected connection.
        // Give each connection a fresh I/O wrapper. First-instance creation
        // remains mandatory; never adopt an endpoint placed in the brief gap.
        drop(pipe);
        let deadline = tokio::time::Instant::now() + EXCHANGE;
        let next = loop {
            match server(&identity, &name) {
                Ok(server) => break Some(server),
                Err(_) if tokio::time::Instant::now() < deadline => {
                    tokio::select! {
                        biased;
                        _=&mut stop=>break None,
                        _=tokio::time::sleep(Duration::from_millis(5))=>{}
                    }
                }
                Err(_) => break None,
            }
        };
        let Some(next) = next else { break };
        pipe = next;
    }
}

async fn exchange(
    pipe: &mut NamedPipeServer,
    sender: &mpsc::SyncSender<Incoming>,
    wake: &Wake,
    accepted: &mut VecDeque<(String, [u8; 32])>,
) -> Result<(), &'static str> {
    let bytes = read_frame(pipe).await.map_err(|_| INVALID)?;
    let request: Request = serde_json::from_slice(&bytes).map_err(|_| INVALID)?;
    let input = action(&request)?;
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    let status = if let Some((_, previous)) = accepted.iter().find(|(id, _)| id == &request.request)
    {
        if previous != &digest {
            return Err(INVALID);
        }
        Status::Accepted
    } else {
        let (reply, response) = oneshot::channel();
        let incoming = Incoming {
            action: input,
            reply,
            deadline: Instant::now() + EXCHANGE,
        };
        let callback = {
            let mut state = wake.lock().map_err(|_| INVALID)?;
            if sender.try_send(incoming).is_err() {
                None
            } else {
                state.pending = true;
                Some(state.callback.clone())
            }
        };
        if let Some(callback) = callback {
            if let Some(notify) = callback {
                notify();
            }
            response.await.unwrap_or(Status::Busy)
        } else {
            Status::Busy
        }
    };
    if status == Status::Accepted && !accepted.iter().any(|(id, _)| id == &request.request) {
        if accepted.len() == 32 {
            accepted.pop_front();
        }
        accepted.push_back((request.request.clone(), digest));
    }
    let response = serde_json::to_vec(&Response {
        version: 1,
        request: request.request.clone(),
        status,
    })
    .map_err(|_| INVALID)?;
    write_frame(pipe, &response).await.map_err(|_| INVALID)?;
    // DisconnectNamedPipe discards unread buffered bytes. A bounded receipt
    // proves the client read our response; FlushFileBuffers could block forever.
    let receipt = read_frame(pipe).await.map_err(|_| INVALID)?;
    let receipt: Receipt = serde_json::from_slice(&receipt).map_err(|_| INVALID)?;
    if receipt.version != 1 || receipt.request != request.request || !receipt.received {
        return Err(INVALID);
    }
    Ok(())
}

fn client(name: &str) -> Result<NamedPipeClient, io::Error> {
    let name = instance_identity::wide(name);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            instance_identity::PIPE_ACCESS,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            HANDLE::default(),
        )
    }
    .map_err(|error| io::Error::from_raw_os_error(error.code().0 & 0xffff))?;
    let handle = unsafe { instance_identity::own(handle) };
    // Tokio assumes ownership even if registration with the I/O driver fails.
    unsafe { NamedPipeClient::from_raw_handle(handle.into_raw_handle()) }
}

fn forward(
    identity: &Identity,
    pipe: &str,
    invitation: Option<Invitation>,
    focus: bool,
) -> Result<(), &'static str> {
    let mut random = [0u8; 16];
    getrandom::getrandom(&mut random).map_err(|_| INVALID)?;
    let id = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let request = Request {
        version: 1,
        request: id.clone(),
        invitation: invitation
            .map(|invite| serde_json::Value::String(invite.link()))
            .unwrap_or(serde_json::Value::Null),
    };
    action(&request)?;
    let body = serde_json::to_vec(&request).map_err(|_| INVALID)?;
    runtime()?.block_on(async {
        tokio::time::timeout(Duration::from_secs(5),async {
            let mut pipe=loop {
                match client(pipe) {
                    Ok(pipe)=>break pipe,
                    Err(error) if matches!(error.raw_os_error(),Some(code) if code==ERROR_FILE_NOT_FOUND.0 as i32 || code==ERROR_PIPE_BUSY.0 as i32)=>tokio::time::sleep(Duration::from_millis(25)).await,
                    Err(_)=>return Err(INVALID),
                }
            };
            let peer=identity.peer(HANDLE(pipe.as_raw_handle()),true)?;
            // This grants only the already OS-verified server process. Windows
            // may deny foreground activation; the UI also requests attention.
            if focus {unsafe {let _=AllowSetForegroundWindow(peer.pid);}}
            write_frame(&mut pipe,&body).await.map_err(|_|INVALID)?;
            let bytes=read_frame(&mut pipe).await.map_err(|_|INVALID)?;
            let response:Response=serde_json::from_slice(&bytes).map_err(|_|INVALID)?;
            if response.version!=1 || response.request!=id {return Err(INVALID)}
            let receipt=serde_json::to_vec(&Receipt {version:1,request:id.clone(),received:true}).map_err(|_|INVALID)?;
            write_frame(&mut pipe,&receipt).await.map_err(|_|INVALID)?;
            match response.status {Status::Accepted=>Ok(()),Status::Busy=>Err(BUSY)}
        }).await.map_err(|_|BUSY)?
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn invite() -> Invitation {
        Invitation::new(
            "http://100.70.1.2:4580",
            "4be44aef-8c98-4db6-a9d5-81c2eb311330",
            "owned-fixture-only",
        )
        .unwrap()
    }
    #[test]
    fn strict_frame_rejects_unknown_schema_fields_controls_and_commands() {
        let request = Request {
            version: 1,
            request: "a".repeat(32),
            invitation: serde_json::Value::String(invite().link()),
        };
        assert!(matches!(action(&request), Ok(Action::Open(_))));
        assert!(serde_json::from_value::<Request>(
            serde_json::json!({"version":1,"request":"a".repeat(32)})
        )
        .is_err());
        for value in [
            serde_json::json!({"version":1,"request":"a".repeat(32),"invitation":null,"command":"run"}),
            serde_json::json!({"version":1,"request":"a".repeat(32),"invitation":null,"profile":"C:\\user"}),
        ] {
            assert!(serde_json::from_value::<Request>(value).is_err());
        }
        for request in [
            Request {
                version: 2,
                request: "a".repeat(32),
                invitation: serde_json::Value::Null,
            },
            Request {
                version: 1,
                request: "INVALID".into(),
                invitation: serde_json::Value::Null,
            },
            Request {
                version: 1,
                request: "a".repeat(32),
                invitation: serde_json::Value::String(invite().link() + "\n"),
            },
        ] {
            assert!(action(&request).is_err());
        }
        runtime().unwrap().block_on(async {
            for length in [0u32, 4353, u32::MAX] {
                let input = length.to_le_bytes();
                assert!(read_frame(&mut input.as_slice()).await.is_err());
            }
            assert!(read_frame(&mut [0u8; 3].as_slice()).await.is_err());
            let mut short = 10u32.to_le_bytes().to_vec();
            short.extend_from_slice(b"hi");
            assert!(read_frame(&mut short.as_slice()).await.is_err());
        });
    }
    #[test]
    fn idle_inbox_attachment_does_not_wake_or_restore_a_window() {
        let (_sender, receiver) = mpsc::sync_channel(1);
        let wake: Wake = Arc::new(Mutex::new(WakeState::default()));
        let inbox = Inbox {
            receiver,
            wake: wake.clone(),
        };
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        inbox.attach(move || {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        assert!(inbox.next().is_none());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!wake.lock().unwrap().pending);
    }
    #[test]
    fn pre_ui_request_wakes_on_attachment_once_and_callback_holds_no_queue_lock() {
        let scope = uuid::Uuid::new_v4().to_string();
        let Start::Owner(owner, inbox) = start_scoped(&scope, None, false).unwrap() else {
            panic!("Fresh fixture occupied")
        };
        let caller = thread::spawn(move || {
            start_scoped(&scope, Some(invite()), false).map(|r| matches!(r, Start::Forwarded))
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while !inbox.wake.lock().unwrap().pending {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        let coordination = inbox.wake.clone();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        inbox.attach(move || {
            assert!(
                coordination.try_lock().is_ok(),
                "UI callbacks must not run under the coordination lock"
            );
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let incoming = inbox.next().unwrap();
        assert!(incoming.current());
        incoming.respond(Offer::Accepted);
        assert!(caller.join().unwrap().unwrap());
        let count = calls.clone();
        inbox.attach(move || {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        drop(owner);
    }
    #[test]
    fn actual_pipe_roundtrip_requires_ui_acceptance_and_releases_ownership() {
        let scope = uuid::Uuid::new_v4().to_string();
        let Start::Owner(owner, inbox) = start_scoped(&scope, None, false).unwrap() else {
            panic!("Fresh fixture occupied")
        };
        let (wake, notified) = mpsc::channel();
        inbox.attach(move || {
            let _ = wake.send(());
        });
        let other = scope.clone();
        let caller = thread::spawn(move || {
            start_scoped(&other, Some(invite()), false)
                .map(|result| matches!(result, Start::Forwarded))
        });
        notified.recv_timeout(Duration::from_secs(3)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let incoming = loop {
            if let Some(incoming) = inbox.next() {
                break incoming;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        };
        assert!(incoming.current());
        assert!(matches!(&incoming.action,Action::Open(value) if value==&invite()));
        assert!(!caller.is_finished());
        incoming.respond(Offer::Accepted);
        assert!(caller.join().unwrap().unwrap());
        drop(owner);
        drop(inbox);
        assert!(matches!(
            start_scoped(&scope, None, false).unwrap(),
            Start::Owner(_, _)
        ));
    }
    #[test]
    fn shutdown_cancels_a_connected_hanging_client_without_waiting_for_its_payload() {
        let scope = uuid::Uuid::new_v4().to_string();
        let Start::Owner(owner, _inbox) = start_scoped(&scope, None, false).unwrap() else {
            panic!("Fresh fixture occupied")
        };
        let identity = Identity::current().unwrap();
        let name = format!(r"\\.\pipe\{}", identity.namespace(&scope));
        let runtime = runtime().unwrap();
        let _enter = runtime.enter();
        let _client = client(&name).unwrap();
        thread::sleep(Duration::from_millis(20));
        let start = Instant::now();
        drop(owner);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn process_fixture_child() {
        let Ok(scope) = std::env::var("RELAY_INVITATION_FIXTURE_SCOPE") else {
            return;
        };
        let scope = uuid::Uuid::parse_str(&scope)
            .expect("Only an owned random fixture namespace")
            .to_string();
        let mode = std::env::var("RELAY_INVITATION_FIXTURE_MODE").unwrap();
        let Start::Owner(owner, inbox) = start_scoped(&scope, None, false).unwrap() else {
            panic!("Owned fixture unavailable")
        };
        use std::io::Write;
        println!("OWNED-INSTANCE-READY");
        std::io::stdout().flush().unwrap();
        if mode == "crash" {
            std::process::exit(17);
        }
        let incoming = inbox.receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(incoming.current());
        assert!(matches!(&incoming.action, Action::Open(value) if value == &invite()));
        incoming.respond(Offer::Accepted);
        let mut stop = String::new();
        std::io::stdin().read_line(&mut stop).unwrap();
        assert_eq!(stop.trim(), "stop");
        drop(owner);
    }

    struct ChildGuard(std::process::Child, Option<thread::JoinHandle<()>>);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
            if let Some(reader) = self.1.take() {
                let _ = reader.join();
            }
        }
    }

    fn child(scope: &str, mode: &str) -> ChildGuard {
        use std::{
            io::Read,
            os::windows::process::CommandExt,
            process::{Command, Stdio},
        };
        let mut process = ChildGuard(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "single_instance::tests::process_fixture_child",
                    "--nocapture",
                ])
                .env("RELAY_INVITATION_FIXTURE_SCOPE", scope)
                .env("RELAY_INVITATION_FIXTURE_MODE", mode)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .creation_flags(0x08000000)
                .spawn()
                .unwrap(),
            None,
        );
        let mut output = process.0.stdout.take().unwrap();
        let (ready, receiver) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut seen = Vec::new();
            let mut byte = [0u8; 1];
            let mut reported = false;
            while seen.len() < 2048 && output.read_exact(&mut byte).is_ok() {
                seen.push(byte[0]);
                if !reported
                    && (seen.ends_with(b"OWNED-INSTANCE-READY\n")
                        || seen.ends_with(b"OWNED-INSTANCE-READY\r\n"))
                {
                    let _ = ready.send(());
                    reported = true;
                }
            }
        });
        if receiver.recv_timeout(Duration::from_secs(5)).is_err() {
            let _ = process.0.kill();
            let _ = process.0.wait();
            let _ = reader.join();
            panic!("Owned headless fixture did not report readiness");
        }
        process.1 = Some(reader);
        process
    }

    #[test]
    fn actual_two_process_handoff_does_not_start_another_owner() {
        use std::io::Write;
        let scope = uuid::Uuid::new_v4().to_string();
        let mut child = child(&scope, "accept");
        assert!(matches!(
            start_scoped(&scope, Some(invite()), false).unwrap(),
            Start::Forwarded
        ));
        child.0.stdin.take().unwrap().write_all(b"stop\n").unwrap();
        assert!(child.0.wait().unwrap().success());
        assert!(matches!(
            start_scoped(&scope, None, false).unwrap(),
            Start::Owner(_, _)
        ));
    }

    #[test]
    fn crashed_process_releases_pipe_and_abandoned_lifetime_mutex() {
        let scope = uuid::Uuid::new_v4().to_string();
        let mut child = child(&scope, "crash");
        assert_eq!(child.0.wait().unwrap().code(), Some(17));
        assert!(matches!(
            start_scoped(&scope, None, false).unwrap(),
            Start::Owner(_, _)
        ));
    }

    #[test]
    fn native_created_pipe_has_logon_acl_and_checks_os_peer_before_payload() {
        let identity = Identity::current().unwrap();
        let name = format!(
            r"\\.\pipe\{}",
            identity.namespace(&uuid::Uuid::new_v4().to_string())
        );
        runtime().unwrap().block_on(async {
            let pipe = server(&identity, &name).unwrap();
            identity.assert_created_pipe_acl(HANDLE(pipe.as_raw_handle()));
            let client = client(&name).unwrap();
            pipe.connect().await.unwrap();
            assert!(identity.peer(HANDLE(client.as_raw_handle()), true).is_ok());
            assert!(identity.peer(HANDLE(pipe.as_raw_handle()), false).is_ok());
            let different = identity.different_fixture_identity();
            assert!(different
                .peer(HANDLE(client.as_raw_handle()), true)
                .is_err());
            assert!(different.peer(HANDLE(pipe.as_raw_handle()), false).is_err());
        });
    }

    #[test]
    fn explicit_ui_busy_is_returned_without_success_or_a_second_owner() {
        let scope = uuid::Uuid::new_v4().to_string();
        let Start::Owner(owner, inbox) = start_scoped(&scope, None, false).unwrap() else {
            panic!("Fresh fixture occupied")
        };
        for index in 0..32 {
            let other = scope.clone();
            let caller = thread::spawn(move || start_scoped(&other, Some(invite()), false).err());
            let incoming = inbox.receiver.recv_timeout(Duration::from_secs(3)).unwrap();
            let offer = if index % 2 == 0 {
                Offer::Busy
            } else {
                Offer::Accepted
            };
            incoming.respond(offer);
            assert_eq!(
                caller.join().unwrap(),
                if offer == Offer::Busy {
                    Some(BUSY)
                } else {
                    None
                }
            );
        }
        drop(owner);
    }

    #[test]
    fn occupied_unknown_endpoint_is_not_adopted_and_does_not_keep_the_mutex() {
        let scope = uuid::Uuid::new_v4().to_string();
        let identity = Identity::current().unwrap();
        let name = format!(r"\\.\pipe\{}", identity.namespace(&scope));
        let runtime = runtime().unwrap();
        let _enter = runtime.enter();
        let unknown = server(&identity, &name).unwrap();
        assert!(start_scoped(&scope, None, false).is_err());
        drop(unknown);
        drop(_enter);
        drop(runtime);
        assert!(matches!(
            start_scoped(&scope, None, false).unwrap(),
            Start::Owner(_, _)
        ));
    }

    #[test]
    fn invalid_native_frame_is_not_queued_and_next_connection_still_works() {
        let scope = uuid::Uuid::new_v4().to_string();
        let Start::Owner(owner, inbox) = start_scoped(&scope, None, false).unwrap() else {
            panic!("Fresh fixture occupied")
        };
        let identity = Identity::current().unwrap();
        let name = format!(r"\\.\pipe\{}", identity.namespace(&scope));
        runtime().unwrap().block_on(async {
            let mut invalid = client(&name).unwrap();
            write_frame(&mut invalid, b"\xff").await.unwrap();
            let result = tokio::time::timeout(EXCHANGE, read_frame(&mut invalid)).await;
            assert!(matches!(result, Ok(Err(_))));
        });
        assert!(inbox.next().is_none());
        let other = scope.clone();
        let caller = thread::spawn(move || {
            start_scoped(&other, Some(invite()), false)
                .map(|result| matches!(result, Start::Forwarded))
        });
        inbox
            .receiver
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .respond(Offer::Accepted);
        assert!(caller.join().unwrap().unwrap());
        drop(owner);
    }

    #[test]
    fn caller_expiry_leaves_no_late_ui_permission_and_shutdown_stays_bounded() {
        let scope = uuid::Uuid::new_v4().to_string();
        let Start::Owner(owner, inbox) = start_scoped(&scope, None, false).unwrap() else {
            panic!("Fresh fixture occupied")
        };
        let other = scope.clone();
        let caller = thread::spawn(move || start_scoped(&other, Some(invite()), false).err());
        let incoming = inbox.receiver.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(caller.join().unwrap().is_some());
        assert!(!incoming.current());
        drop(incoming);
        let start = Instant::now();
        drop(owner);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cold_open_forwards_to_a_clean_owner_instead_of_retaining_bearer_argv() {
        use std::io::Write;
        let scope = uuid::Uuid::new_v4().to_string();
        let mut process = None;
        open_scoped(&scope, invite(), false, || {
            process = Some(child(&scope, "accept"));
            Ok(())
        })
        .unwrap();
        let mut process = process.unwrap();
        process
            .0
            .stdin
            .take()
            .unwrap()
            .write_all(b"stop\n")
            .unwrap();
        assert!(process.0.wait().unwrap().success());
    }
}
