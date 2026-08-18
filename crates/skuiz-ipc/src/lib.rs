//! skuiz-ipc: a message bus for plugin instances, in one process or across
//! several.
//!
//! # Why this is two tiers
//!
//! Plugin instances usually are *not* in separate processes. A CLAP or VST3
//! plugin is a shared library loaded into the DAW, so every instance in a
//! project lives in the host's address space; and on Apple platforms every
//! AUv3 instance of one plugin inside one host shares a single extension
//! process (Apple developer forums, thread 65909 — there is no way to force
//! otherwise). Separate processes are the *exception*: a sandboxing host, or
//! two different applications hosting the same plugin at once.
//!
//! So the bus delivers on whichever tier applies:
//!
//! - **In-process** — a direct call into the other instances' callbacks. No
//!   socket, no serialization, no thread hop, no election.
//! - **Cross-process** — the Unix socket link below, of which there is
//!   exactly **one per process** per scope, no matter how many instances are
//!   loaded. Ten instances in a DAW open one socket between them, not ten.
//!
//! Callers never choose: [`Bus::send`] reaches every other instance
//! wherever it lives.
//!
//! An important consequence for sandboxed hosts: because in-process delivery
//! does not touch the socket, instances inside one host keep syncing even
//! when the socket cannot be created at all — a misconfigured App Group on
//! iOS degrades cross-host sync, rather than silently breaking everything.
//!
//! # Election
//!
//! Exactly one instance, process-wide and machine-wide, reports
//! [`Bus::is_server`]. That is the one that should own writing shared state
//! on project save. It is elected in two steps: an `flock` picks the owning
//! *process* (the kernel releases it if that process dies, so it cannot go
//! stale), and the longest-lived instance within that process is the owner.
//! Both promote automatically — deleting the owning instance hands the role
//! to the next one immediately, with no socket round trip.
//!
//! # Semantics
//!
//! A frame sent by one node reaches every *other* node; the sender never
//! hears its own. Frames are opaque bytes. In-process callbacks run on the
//! sending thread, so keep them short and do not call back into [`Bus::send`]
//! from one. Cross-process callbacks run on a bus thread. Either way the
//! callback must only touch state it owns (an `Arc`), never plugin memory
//! that could be freed underneath it.
//!
//! # Platforms
//!
//! The cross-process tier is a Unix domain socket on macOS and Linux, and a
//! named pipe on Windows; see `src/transport/`. Everything above it — the
//! registry, in-process delivery, election bookkeeping and framing — is
//! shared. The Windows backend type-checks but has not been run; see its
//! module docs.

mod transport;

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;
use std::time::Duration;

const MAX_FRAME: u32 = 1 << 20;

type Callback = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Process-wide table of live groups, so instances sharing a scope find each
/// other directly instead of through the kernel.
fn registry() -> &'static Mutex<HashMap<String, Weak<Group>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Weak<Group>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A handle held by one plugin instance.
pub struct Bus {
    group: Arc<Group>,
    /// Identifies this instance within the group; also its election rank,
    /// since ids increase and the lowest surviving one owns the role.
    id: u64,
}

struct Member {
    id: u64,
    on_message: Callback,
}

/// Everything shared by the instances of one scope inside this process.
struct Group {
    key: String,
    endpoint: transport::Endpoint,
    /// Local instances. Ordered by id, so the first is the longest-lived.
    members: Mutex<Vec<Member>>,
    next_member_id: AtomicU64,
    /// True when this *process* holds the cross-process election lock.
    owns_lock: AtomicBool,
    shutdown: AtomicBool,
    /// Client role: this process's connection to the server process.
    tx: Mutex<Option<transport::Conn>>,
    /// Server role: connections from other processes, keyed so a relayed
    /// frame can skip its originator.
    clients: Mutex<Vec<(u64, transport::Conn)>>,
    next_conn_id: AtomicU64,
}

impl Bus {
    /// Join (or create) the bus for `scope` — typically the plugin id.
    ///
    /// `on_message` is called for every frame sent by another instance; hand
    /// it `Arc`-owned state only.
    pub fn join(scope: &str, on_message: impl Fn(&[u8]) + Send + Sync + 'static) -> Bus {
        Self::join_in(&std::env::temp_dir(), scope, on_message)
    }

    /// Join the bus for `scope` with the cross-process socket placed in `dir`.
    ///
    /// Sandboxed hosts need this: a macOS or iOS AUv3 extension cannot reach
    /// the shared temp directory, so point `dir` at an App Group container
    /// (`containerURL(forSecurityApplicationGroupIdentifier:)`) to sync with
    /// instances in *other* hosts. Instances within one host share a process
    /// and sync regardless of what `dir` says.
    pub fn join_in(
        dir: &Path,
        scope: &str,
        on_message: impl Fn(&[u8]) + Send + Sync + 'static,
    ) -> Bus {
        let sane: String = scope
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let key = format!("{}\u{0}{}", dir.display(), sane);

        let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        // Reuse a live group; a group already shutting down must not be
        // handed out, or the newcomer would join a bus that is going away.
        let group = match reg.get(&key).and_then(Weak::upgrade) {
            Some(g) if !g.shutdown.load(Ordering::Acquire) => g,
            _ => {
                let group = Arc::new(Group {
                    key: key.clone(),
                    endpoint: transport::Endpoint::new(dir, &sane),
                    members: Mutex::new(Vec::new()),
                    next_member_id: AtomicU64::new(0),
                    owns_lock: AtomicBool::new(false),
                    shutdown: AtomicBool::new(false),
                    tx: Mutex::new(None),
                    clients: Mutex::new(Vec::new()),
                    next_conn_id: AtomicU64::new(0),
                });
                reg.insert(key, Arc::downgrade(&group));
                let lc = Arc::clone(&group);
                thread::spawn(move || lifecycle(lc));
                group
            }
        };
        drop(reg);

        let id = group.next_member_id.fetch_add(1, Ordering::Relaxed);
        group
            .members
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Member {
                id,
                on_message: Arc::new(on_message),
            });
        Bus { group, id }
    }

    /// Deliver `msg` to every other instance, in this process and beyond.
    ///
    /// In-process delivery is immediate; the cross-process hop is a socket
    /// write, so call this from the UI/main thread, not the audio thread.
    pub fn send(&self, msg: &[u8]) {
        self.group.deliver_locally(msg, Some(self.id));
        self.group.send_remote(msg);
    }

    /// Whether this instance is *the* owner: the longest-lived instance in
    /// the process holding the cross-process election lock. Exactly one
    /// instance on the machine reports true.
    pub fn is_server(&self) -> bool {
        self.group.owns_lock.load(Ordering::Acquire) && self.group.owner_id() == Some(self.id)
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let mut members = self.group.members.lock().unwrap_or_else(|e| e.into_inner());
        members.retain(|m| m.id != self.id);
        let last_one_out = members.is_empty();
        drop(members);

        if !last_one_out {
            // Other instances remain in this process: the socket link stays
            // up, and the next member has already inherited the role.
            return;
        }

        // No instances left here: tear the process's link down so another
        // process can take the election lock.
        self.group.shutdown.store(true, Ordering::Release);
        if let Some(tx) = self
            .group
            .tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            tx.close();
        }
        for (_, c) in self
            .group
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            c.close();
        }
        // Unblock the accept loop if this process is serving.
        transport::wake_listener(&self.group.endpoint);

        // Drop only our own entry: a later join may already have replaced
        // it with a fresh group under the same key.
        let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        let ours = Arc::downgrade(&self.group);
        if reg
            .get(&self.group.key)
            .is_some_and(|w| Weak::ptr_eq(w, &ours))
        {
            reg.remove(&self.group.key);
        }
    }
}

impl Group {
    /// The longest-lived local instance, which owns the role when this
    /// process holds the lock.
    fn owner_id(&self) -> Option<u64> {
        self.members
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|m| m.id)
            .min()
    }

    /// Fan a frame out to local instances, optionally skipping the sender.
    ///
    /// Callbacks are cloned out before any is invoked: holding the member
    /// lock across a callback would deadlock the moment one touched the bus.
    fn deliver_locally(&self, msg: &[u8], skip: Option<u64>) {
        let targets: Vec<Callback> = {
            let members = self.members.lock().unwrap_or_else(|e| e.into_inner());
            members
                .iter()
                .filter(|m| Some(m.id) != skip)
                .map(|m| Arc::clone(&m.on_message))
                .collect()
        };
        for cb in targets {
            cb(msg);
        }
    }

    /// Push a frame to the other processes on this scope, if any.
    fn send_remote(&self, msg: &[u8]) {
        if self.owns_lock.load(Ordering::Acquire) {
            self.broadcast_except(None, msg);
        } else if let Some(tx) = self.tx.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = write_frame(tx, msg);
        }
        // ponytail: frames sent during an election window (no link yet) are
        // dropped; queue them if that gap ever matters.
    }

    fn broadcast_except(&self, skip: Option<u64>, msg: &[u8]) {
        // Dead peers are detected by write failure and dropped.
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain_mut(|(id, tx)| Some(*id) == skip || write_frame(tx, msg).is_ok());
    }
}

fn write_frame(w: &mut impl Write, data: &[u8]) -> std::io::Result<()> {
    w.write_all(&(data.len() as u32).to_le_bytes())?;
    w.write_all(data)
}

fn read_frame(r: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// One per process per scope: win the lock and serve, or connect as a client.
fn lifecycle(group: Arc<Group>) {
    while !group.shutdown.load(Ordering::Acquire) {
        match transport::try_become_server(&group.endpoint) {
            Some(listener) => {
                group.owns_lock.store(true, Ordering::Release);
                run_server(&group, listener);
                group.owns_lock.store(false, Ordering::Release);
                // Dropping the listener releases the election.
                transport::release(&group.endpoint);
            }
            None => match transport::connect(&group.endpoint) {
                Some(stream) => run_client(&group, stream),
                // The winner holds the name but has not begun listening
                // yet, or the location is unusable (a sandbox denial).
                // Either way, in-process delivery is unaffected.
                None => thread::sleep(Duration::from_millis(20)),
            },
        }
    }
}

fn run_server(group: &Arc<Group>, listener: transport::Listener) {
    while let Some(conn) = listener.accept() {
        if group.shutdown.load(Ordering::Acquire) {
            return;
        }
        let Some(reader) = conn.try_clone() else {
            continue;
        };
        let id = group.next_conn_id.fetch_add(1, Ordering::Relaxed);
        group
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, conn));
        let group = Arc::clone(group);
        thread::spawn(move || serve_conn(&group, id, reader));
    }
}

fn serve_conn(group: &Group, conn_id: u64, mut recv: transport::Conn) {
    while let Ok(frame) = read_frame(&mut recv) {
        if group.shutdown.load(Ordering::Acquire) {
            return;
        }
        // Came from another process: every local instance should hear it,
        // and every *other* process too.
        group.deliver_locally(&frame, None);
        group.broadcast_except(Some(conn_id), &frame);
        // ponytail: frames are relayed verbatim; the map/reduce hook from
        // PLAN.md slots in here when an example needs aggregation.
    }
    let mut clients = group.clients.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(pos) = clients.iter().position(|(id, _)| *id == conn_id) {
        let (_, c) = clients.remove(pos);
        c.close();
    }
}

fn run_client(group: &Group, stream: transport::Conn) {
    let Some(mut recv) = stream.try_clone() else {
        return;
    };
    *group.tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(stream);
    while let Ok(frame) = read_frame(&mut recv) {
        if group.shutdown.load(Ordering::Acquire) {
            break;
        }
        group.deliver_locally(&frame, None);
    }
    if let Some(tx) = group.tx.lock().unwrap_or_else(|e| e.into_inner()).take() {
        tx.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    /// Poll for a condition rather than sleeping a fixed time: these tests
    /// run alongside the rest of the suite, where a fixed delay is either
    /// flaky under load or needlessly slow.
    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if cond() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}");
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("skuiz-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn in_process_instances_talk_directly() {
        let dir = scratch("inproc");
        let scope = format!("inproc-{}", std::process::id());

        let a_got = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let b_got = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));

        let a = {
            let got = Arc::clone(&a_got);
            Bus::join_in(&dir, &scope, move |m| got.lock().unwrap().push(m.to_vec()))
        };
        let b = {
            let got = Arc::clone(&b_got);
            Bus::join_in(&dir, &scope, move |m| got.lock().unwrap().push(m.to_vec()))
        };

        // No waiting: same-process delivery is a direct call, so the frame
        // has already landed by the time send() returns.
        b.send(b"hello from b");
        assert_eq!(
            a_got.lock().unwrap().as_slice(),
            &[b"hello from b".to_vec()]
        );
        assert!(
            b_got.lock().unwrap().is_empty(),
            "sender must not hear its own frame"
        );

        a.send(b"hello from a");
        assert_eq!(
            b_got.lock().unwrap().as_slice(),
            &[b"hello from a".to_vec()]
        );
        assert_eq!(a_got.lock().unwrap().len(), 1);

        // One socket for the whole process, not one per instance: with both
        // instances local, the process has no peer connections at all.
        wait_until("the process link to come up", || a.is_server());
        assert!(!b.is_server(), "exactly one instance owns the role");
        assert!(
            a.group.clients.lock().unwrap().is_empty(),
            "local instances must not open socket connections to each other"
        );
        assert!(
            a.group.tx.lock().unwrap().is_none(),
            "the serving process must not also be a client of itself"
        );
        assert!(
            Arc::ptr_eq(&a.group, &b.group),
            "instances must share one group"
        );

        drop(a);
        // Promotion within a process is immediate: no re-election needed.
        assert!(
            b.is_server(),
            "the next instance must inherit the role at once"
        );
        drop(b);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn in_process_sync_survives_an_unusable_socket_directory() {
        // The sandbox failure mode: no App Group configured, so the socket
        // path cannot be created. Instances inside the host must still sync
        // with each other rather than silently going deaf.
        let dir = PathBuf::from("/skuiz-nonexistent-directory-for-tests");
        assert!(!dir.exists(), "test needs an unusable directory");
        let scope = format!("nodir-{}", std::process::id());

        let got = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let a = {
            let got = Arc::clone(&got);
            Bus::join_in(&dir, &scope, move |m| got.lock().unwrap().push(m.to_vec()))
        };
        let b = Bus::join_in(&dir, &scope, |_| {});

        b.send(b"still works");
        assert_eq!(
            got.lock().unwrap().as_slice(),
            &[b"still works".to_vec()],
            "in-process delivery must not depend on the socket"
        );
        // Nobody can hold a lock that cannot be created.
        assert!(!a.is_server());
        assert!(!b.is_server());
    }

    #[test]
    fn socket_directory_is_configurable() {
        let dir = scratch("dir");
        let scope = format!("dirtest-{}", std::process::id());
        let a = Bus::join_in(&dir, &scope, |_| {});
        wait_until("the socket to appear in the requested directory", || {
            dir.join(format!("skuiz-{scope}.sock")).exists()
        });
        assert!(a.is_server());
        drop(a);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unix-only by nature: a Windows pipe name is a kernel object that
    /// disappears with its owner, so it cannot be left stale in the first
    /// place.
    #[cfg(unix)]
    #[test]
    fn stale_socket_does_not_split_the_bus() {
        use std::os::unix::net::UnixListener;
        // Simulates a crashed server: a socket file on disk that nothing is
        // listening on. The next process must clear it and serve.
        let dir = scratch("stale");
        let scope = format!("stale-{}", std::process::id());
        let sock = dir.join(format!("skuiz-{scope}.sock"));

        drop(UnixListener::bind(&sock).unwrap());
        assert!(sock.exists(), "test needs a stale socket to be meaningful");

        let a = Bus::join_in(&dir, &scope, |_| {});
        wait_until("recovery from a stale socket", || a.is_server());

        drop(a);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of `crosses_process_boundary`, run as a child process.
    /// Ignored so it never runs on its own.
    #[test]
    #[ignore]
    fn cross_process_child() {
        let dir = PathBuf::from(std::env::var("SKUIZ_TEST_DIR").expect("SKUIZ_TEST_DIR"));
        let scope = std::env::var("SKUIZ_TEST_SCOPE").expect("SKUIZ_TEST_SCOPE");

        let heard = Arc::new(AtomicUsize::new(0));
        let bus = {
            let heard = Arc::clone(&heard);
            Bus::join_in(&dir, &scope, move |_| {
                heard.fetch_add(1, Ordering::SeqCst);
            })
        };
        // Keep announcing until the parent acknowledges by replying.
        for _ in 0..200 {
            bus.send(b"from child");
            if heard.load(Ordering::SeqCst) > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        assert!(
            heard.load(Ordering::SeqCst) > 0,
            "child never heard the parent"
        );
    }

    #[test]
    fn crosses_process_boundary() {
        // In-process delivery short-circuits the socket, so this is the only
        // test that proves the cross-process tier still carries traffic.
        let dir = scratch("xproc");
        let scope = format!("xproc-{}", std::process::id());

        let got = Arc::new(AtomicUsize::new(0));
        let parent = {
            let got = Arc::clone(&got);
            Bus::join_in(&dir, &scope, move |_| {
                got.fetch_add(1, Ordering::SeqCst);
            })
        };
        wait_until("the parent to take the election lock", || {
            parent.is_server()
        });

        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::cross_process_child",
                "--ignored",
                "--nocapture",
            ])
            .env("SKUIZ_TEST_DIR", &dir)
            .env("SKUIZ_TEST_SCOPE", &scope)
            .spawn()
            .expect("spawn child test process");

        wait_until("a frame from another process", || {
            // Reply so the child can confirm the link both ways.
            parent.send(b"from parent");
            got.load(Ordering::SeqCst) > 0
        });

        let status = child.wait().expect("child exited");
        assert!(status.success(), "child process failed: {status:?}");

        drop(parent);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
