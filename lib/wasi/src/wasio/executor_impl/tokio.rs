//! A WASIO executor backed by `tokio`.

use super::super::executor::{self, Executor};
use super::super::socket::*;
use super::super::types::*;
use crate::syscalls::types::*;
use crate::{ptr::WasmPtr, Fd, WasiFile, WasiFs, WasiFsError, ALL_RIGHTS, VIRTUAL_ROOT_FD};
use wasmer::{Memory, Array};
use crossbeam::channel::{unbounded, Receiver, Sender};
use flurry::HashMap as ConcHashMap;
use futures::future::{ready, AbortHandle, Abortable, Future, FutureExt, TryFutureExt};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{
    tcp::{OwnedReadHalf, OwnedWriteHalf},
    TcpListener, TcpStream,
};
use tokio::runtime::Runtime;
use tokio::stream::StreamExt;
use tokio::sync::{mpsc, Mutex as AsyncMutex, RwLock as AsyncRwLock};
use std::any::Any;
use std::os::unix::io::{RawFd, AsRawFd};

/// An executor backed by the Tokio runtime.
///
/// Implements the `Executor` trait.
pub struct TokioExecutor {
    /// State of this executor.
    ///
    /// Wrapped in an `Arc` because asynchronous tasks may refer to this.
    state: Arc<ExecutorState>,

    /// The Tokio runtime dedicated to this executor.
    ///
    /// The runtime is backed by a threaded executor.
    runtime: Runtime,

    /// Thread-local completion queue.
    ///
    /// While asynchronously completed operations send their operations to `state.completed_rx`,
    /// this is used for the "fast path" where the operation doesn't really need to be sent to the
    /// asynchronous executor.
    ///
    /// Useful when e.g. data to read is already available and can be instantly copied to Wasm buffer.
    local_completion: RefCell<VecDeque<(__wasi_errno_t, UserContext)>>,

    /// Connection accept queue (RX).
    ///
    /// This is the receiver side of `state.accepted_tx`.
    ///
    /// TODO: One `accepted_rx` per listener socket?
    accepted_rx: RefCell<mpsc::Receiver<(TcpStream, SocketAddr)>>,
}

/// Inner holder of executor state.
struct ExecutorState {
    /// Abort handles for all ongoing operations.
    ///
    /// The index is the `CancellationToken` and is unique across the whole lifetime of the `ExecutorState`.
    ongoing: ConcHashMap<u64, AbortHandle>,

    /// Next index for `ongoing`. Starts from 1 and monotonically increases by 1 per
    /// new handle inserted into `ongoing`.
    ongoing_next: AtomicU64,

    /// Completed operations (TX). (return_value, user_context)
    completed_tx: Sender<(__wasi_errno_t, UserContext)>,

    /// Completed operations (RX). (return_value, user_context)
    completed_rx: Receiver<(__wasi_errno_t, UserContext)>,

    /// Connection accept queue (TX).
    accepted_tx: mpsc::Sender<(TcpStream, SocketAddr)>,
}

impl TokioExecutor {
    /// Creates a `TokioExecutor`.
    pub fn new() -> TokioExecutor {
        let (completed_tx, completed_rx) = unbounded();

        // Capacity must be 1. See `AsyncStreamOperation::SocketListen`.
        let (accepted_tx, accepted_rx) = mpsc::channel(1);

        TokioExecutor {
            state: Arc::new(ExecutorState {
                ongoing: ConcHashMap::new(),
                ongoing_next: AtomicU64::new(1), // index 0 is reserved for "null"
                completed_tx,
                completed_rx,
                accepted_tx,
            }),
            runtime: Runtime::new().expect("TokioExecutor::new: cannot create Tokio runtime"), // XXX: When does the error happen?
            local_completion: RefCell::new(VecDeque::new()),
            accepted_rx: RefCell::new(accepted_rx),
        }
    }
}

impl TokioExecutor {
    /// Spawns a oneshot operation.
    fn spawn_oneshot(
        &self,
        fut: impl Future<Output = __wasi_errno_t> + Send + 'static,
        user_context: UserContext,
    ) -> CancellationToken {
        // Prepare abort handle for cancellation.
        let (abort_handle, abort_registration) = AbortHandle::new_pair();

        // Insert the operation into ongoing.
        let ongoing_index = self.state.ongoing_next.fetch_add(1, Ordering::Relaxed);
        self.state.ongoing.pin().insert(ongoing_index, abort_handle);
        let cancel_token = CancellationToken(ongoing_index as _);

        // Spawn an asynchronous task.
        let state = self.state.clone();
        self.runtime.spawn(async move {
            // Wrap with abort registration.
            let result = Abortable::new(fut, abort_registration).await;

            // Cancellation token is unique. No race condition here.
            state.ongoing.pin().remove(&ongoing_index);

            // Prepare to enqueue the result.
            let enqueued = match result {
                Ok(return_value) => {
                    // Operation finished before any cancellation request.
                    (return_value, user_context)
                }
                Err(_) => {
                    // Operation aborted/cancelled.
                    (__WASI_ECANCELED, user_context)
                }
            };

            // TX and RX are in the same struct (so we know the RX side isn't dropped), so this should never fail.
            state.completed_tx.send(enqueued).unwrap()
        });
        cancel_token
    }
}

impl Executor for TokioExecutor {
    fn enqueue_oneshot(
        &self,
        op: AsyncOneshotOperation,
        user_context: UserContext,
    ) -> Result<CancellationToken, __wasi_errno_t> {
        let imm_result: __wasi_errno_t = match op {
            AsyncOneshotOperation::Nop => 0,
            AsyncOneshotOperation::Delay(duration) => {
                // Spawn a task to notify us after `duration`.
                // `runtime.enter` is needed here because `delay_for` expects to be called within a Tokio runtime context.
                return Ok(self.spawn_oneshot(
                    self.runtime
                        .enter(|| tokio::time::delay_for(duration).map(|_| __WASI_ESUCCESS)),
                    user_context,
                ));
            }
            AsyncOneshotOperation::Read {
                memory,
                fs,
                fd,
                ri_data,
                ri_data_len,
                ri_flags,
                ro_datalen,
                ro_flags,
            } => {
                let sock: AbstractTcpSocket = fs.get_wasi_file_as::<AbstractTcpSocket>(fd)?.clone();

                // Read iovecs into local buffers, to allow the caller to deallocate them.
                let iovecs: Vec<_> = ri_data
                    .deref(memory, 0, ri_data_len)?
                    .iter()
                    .map(|x| x.get())
                    .collect();

                // FIXME: `memory` is marked as `Clone` but might not be actually be safe to clone: `grow()`.
                let memory = memory.clone();

                return Ok(
                    self.spawn_oneshot(
                        self.runtime.enter(|| async move {
                            let sock_inner = sock.inner.read().await;
                            match *sock_inner {
                                AbstractTcpSocketInner::Stream(ref read_half, _, _) => {
                                    let mut read_len: usize = 0;
                                    for v in iovecs {
                                        // FIXME: The vectored read semantics here is correct for streaming protocols (TCP) but not for packet
                                        // protocols (UDP, ICMP, etc.).
                                        let read_buffer = unsafe {
                                            std::mem::transmute::<
                                                &mut [std::cell::Cell<u8>],
                                                &mut [u8],
                                            >(
                                                match v.buf.deref_mut(&memory, 0, v.buf_len) {
                                                    Ok(x) => x,
                                                    Err(e) => return __WASI_EFAULT,
                                                },
                                            )
                                        };
                                        let mut read_half = read_half.lock().await;
                                        match read_half.read(read_buffer).await {
                                            Ok(n) => {
                                                read_len += n;
                                            }
                                            Err(e) => return from_tokio_error(e),
                                        }
                                    }

                                    let ro_datalen = match ro_datalen.deref(&memory) {
                                        Ok(x) => x,
                                        Err(e) => return __WASI_EFAULT,
                                    };

                                    ro_datalen.set(read_len as u32);
                                    __WASI_ESUCCESS
                                }
                                _ => __WASI_EINVAL,
                            }
                        }),
                        user_context,
                    ),
                );
            }
            AsyncOneshotOperation::Write {
                memory,
                fs,
                fd,
                si_data,
                si_data_len,
                si_flags,
                so_datalen,
            } => {
                let sock: AbstractTcpSocket = fs.get_wasi_file_as::<AbstractTcpSocket>(fd)?.clone();

                // Read iovecs into local buffers, to allow the caller to deallocate them.
                let iovecs: Vec<_> = si_data
                    .deref(memory, 0, si_data_len)?
                    .iter()
                    .map(|x| x.get())
                    .collect();

                // FIXME: `memory` is marked as `Clone` but might not be actually be safe to clone: `grow()`.
                let memory = memory.clone();

                return Ok(self.spawn_oneshot(
                    self.runtime.enter(|| async move {
                        let mut data_to_write: Vec<u8> = vec![];

                        for v in iovecs {
                            let data = match v.buf.deref(&memory, 0, v.buf_len) {
                                Ok(x) => x,
                                Err(e) => return __WASI_EFAULT,
                            };
                            for b in data {
                                data_to_write.push(b.get());
                            }
                        }

                        let sock_inner = sock.inner.read().await;
                        match *sock_inner {
                            AbstractTcpSocketInner::Stream(_, ref write_half, _) => {
                                let mut write_half = write_half.lock().await;
                                match sock.run_io_maybe_nb(write_half.write(&data_to_write)).await {
                                    Ok(n) => {
                                        let so_datalen = match so_datalen.deref(&memory) {
                                            Ok(x) => x,
                                            Err(e) => return __WASI_EFAULT,
                                        };

                                        so_datalen.set(n as u32);
                                        __WASI_ESUCCESS
                                    }
                                    Err(e) => from_tokio_error(e),
                                }
                            }
                            _ => __WASI_EINVAL,
                        }
                    }),
                    user_context,
                ));
            }
            AsyncOneshotOperation::Read {
                memory,
                fs,
                fd,
                ri_data,
                ri_data_len,
                ri_flags,
                ro_datalen,
                ro_flags,
            } => {
                let sock: AbstractTcpSocket = fs.get_wasi_file_as::<AbstractTcpSocket>(fd)?.clone();

                // Read iovecs into local buffers, to allow the caller to deallocate them.
                let iovecs: Vec<_> = ri_data
                    .deref(memory, 0, ri_data_len)?
                    .iter()
                    .map(|x| x.get())
                    .collect();

                // FIXME: `memory` is marked as `Clone` but might not be actually be safe to clone: `grow()`.
                let memory = memory.clone();

                return Ok(self.spawn_oneshot(
                    self.runtime.enter(|| async move {
                        // FIXME: Allow multiple iovec elements
                        if iovecs.len() == 0 {
                            let ro_datalen = match ro_datalen.deref(&memory) {
                                Ok(x) => x,
                                Err(e) => return __WASI_EFAULT,
                            };
                            ro_datalen.set(0);
                            return __WASI_ESUCCESS;
                        }
                        let iov = &iovecs[0];

                        let data = match unsafe { iov.buf.deref_mut(&memory, 0, iov.buf_len) } {
                            Ok(x) => x,
                            Err(e) => return __WASI_EFAULT,
                        };
                        let data =
                            unsafe { std::mem::transmute::<&mut [Cell<u8>], &mut [u8]>(data) };

                        let sock_inner = sock.inner.read().await;
                        match *sock_inner {
                            AbstractTcpSocketInner::Stream(ref read_half, _, _) => {
                                let mut read_half = read_half.lock().await;
                                match sock.run_io_maybe_nb(read_half.read(data)).await {
                                    Ok(n) => {
                                        let ro_datalen = match ro_datalen.deref(&memory) {
                                            Ok(x) => x,
                                            Err(e) => return __WASI_EFAULT,
                                        };
                                        ro_datalen.set(n as u32);
                                        __WASI_ESUCCESS
                                    }
                                    Err(e) => from_tokio_error(e),
                                }
                            }
                            _ => __WASI_EINVAL,
                        }
                    }),
                    user_context,
                ));
            }
            AsyncOneshotOperation::SocketPreAccept { fs, fd } => {
                let sock: AbstractTcpSocket = fs.get_wasi_file_as::<AbstractTcpSocket>(fd)?.clone();

                let state = self.state.clone();
                return Ok(self.spawn_oneshot(
                    async move {
                        loop {
                            let sock_inner = sock.inner.read().await;
                            let mut listener = match *sock_inner {
                                AbstractTcpSocketInner::Listening(ref x, _) => x.lock().await,
                                _ => break __WASI_EINVAL,
                            };
                            let (stream, addr) = match listener.accept().await {
                                Ok(x) => x,
                                Err(e) => break from_tokio_error(e),
                            };
                            drop(listener);
                            drop(sock_inner);

                            // Here the capacity of `accepted_tx/rx` is 1. This "serializes" the order of sending
                            // to two different queues from different threads.
                            match state.accepted_tx.clone().send((stream, addr)).await {
                                Ok(()) => {}
                                Err(_) => {
                                    // `state` outlives `self`.
                                    break __WASI_EAGAIN;
                                }
                            }
                            break 0;
                        }
                    },
                    user_context,
                ));
            }
            AsyncOneshotOperation::SocketConnect { memory, fs, fd, sockaddr_ptr, sockaddr_size } => {
                let sock: AbstractTcpSocket = fs.get_wasi_file_as::<AbstractTcpSocket>(fd)?.clone();
                let state = self.state.clone();
                let addr = decode_socket_addr(memory, sockaddr_ptr, sockaddr_size)?;

                return Ok(self.spawn_oneshot(
                    async move {
                        loop {
                            let mut sock_inner = sock.inner.write().await;
                            match *sock_inner {
                                AbstractTcpSocketInner::Undefined4
                                    | AbstractTcpSocketInner::Undefined6
                                    | AbstractTcpSocketInner::Binded(_) => {}
                                _ => break __WASI_EINVAL,
                            }
                            let stream = match TcpStream::connect(addr).await {
                                Ok(x) => x,
                                Err(e) => break from_tokio_error(e)
                            };
                            let fd = stream.as_raw_fd();
                            let md = SocketMetadata {
                                fd,
                                local_address: match stream.local_addr() {
                                    Ok(x) => x,
                                    Err(e) => break from_tokio_error(e),
                                },
                                remote_address: match stream.peer_addr() {
                                    Ok(x) => Some(x),
                                    Err(e) => break from_tokio_error(e),
                                },
                            };
                            let (r, w) = stream.into_split();
                            *sock_inner = AbstractTcpSocketInner::Stream(AsyncMutex::new(r), AsyncMutex::new(w), md);
                            break 0;
                        }
                    },
                    user_context
                ));
            }
            AsyncOneshotOperation::DnsLookup { memory, name, family, output_ptr, output_count_ptr, output_size } => {
                let name = format!("{}:0", name);
                let memory = memory.clone();

                return Ok(self.spawn_oneshot(
                    async move {
                        loop {
                            let results = match tokio::net::lookup_host(&name).await {
                                Ok(x) => x,
                                Err(e) => break from_tokio_error(e), 
                            };
                            let output = match output_ptr.deref(&memory, 0, output_size) {
                                Ok(x) => x,
                                Err(e) => break e,
                            };
                            let output_count = match output_count_ptr.deref(&memory) {
                                Ok(x) => x,
                                Err(e) => break e,
                            };
                            let mut count: u32 = 0;
                            for addr in results {
                                if count == output_size {
                                    break;
                                }
                                match addr {
                                    SocketAddr::V4(x) if family == AF_INET as _ => {
                                        output[count as usize].set(u32::from(*x.ip()) as u128);
                                        count += 1;
                                    }
                                    SocketAddr::V6(x) if family == AF_INET6 as _ => {
                                        output[count as usize].set(u128::from(*x.ip()));
                                        count += 1;
                                    }
                                    _ => {}
                                }
                            }
                            output_count.set(count);
                            break 0;
                        }
                    },
                    user_context
                ));
            }
        };
        self.local_completion
            .borrow_mut()
            .push_back((imm_result, user_context));
        return Ok(CancellationToken(0));
    }

    fn enqueue_stream(
        &self,
        op: AsyncStreamOperation,
        user_context: UserContext,
    ) -> Result<CancellationToken, __wasi_errno_t> {
        match op {}
    }

    fn perform(&self, op: SyncOperation) -> Result<(), __wasi_errno_t> {
        match op {
            SyncOperation::Cancel(cancel_token) => {
                // Attempt to get the abort handle corresponding to the cancellation token.
                //
                // Fails if the associated task has already finished or is cancelled before. Since
                // the cancellation token is unique, racing with completion here is fine.
                if let Some(abort_handle) = self.state.ongoing.pin().remove(&(cancel_token.0 as _))
                {
                    // Send abort signal.
                    abort_handle.abort();

                    Ok(())
                } else {
                    Err(__WASI_EINVAL)
                }
            }
            SyncOperation::SocketCreate(memory, fd_out, fs, domain, ty, _protocol) => {
                let fd_out_cell = fd_out.deref(memory)?;
                let socket = AbstractTcpSocket {
                    inner: Arc::new(AsyncRwLock::new(match (domain, ty) {
                        (x, y) if x == AF_INET && y == SOCK_STREAM => {
                            AbstractTcpSocketInner::Undefined4
                        }
                        (x, y) if x == AF_INET6 && y == SOCK_STREAM => {
                            AbstractTcpSocketInner::Undefined6
                        }
                        _ => return Err(__WASI_EINVAL),
                    })),
                    flags: 0,
                };
                let file = Box::new(socket);
                let socket_name = format!("<socket:{}>", uuid::Uuid::new_v4());
                let fd = fs
                    .open_file_at(
                        VIRTUAL_ROOT_FD,
                        file,
                        Fd::READ | Fd::WRITE,
                        socket_name,
                        ALL_RIGHTS,
                        ALL_RIGHTS,
                        0,
                    )
                    .map_err(|_| __WASI_EINVAL)?;
                fd_out_cell.set(fd);
                Ok(())
            }
            SyncOperation::SocketBind(memory, fs, fd, sockaddr_ptr, sockaddr_size) => {
                let sock: &AbstractTcpSocket = fs.get_wasi_file_as::<AbstractTcpSocket>(fd)?;
                self.runtime.handle().block_on(async {
                    let mut sock_inner = sock.inner.write().await;
                    match (sockaddr_size, &*sock_inner) {
                        (16, AbstractTcpSocketInner::Undefined4) => {
                            let sockaddr = WasmPtr::<SockaddrIn>::new(sockaddr_ptr.offset())
                                .deref(memory)?
                                .get();
                            let ipaddr = Ipv4Addr::from(sockaddr.sin_addr);
                            *sock_inner =
                                AbstractTcpSocketInner::Binded(SocketAddr::V4(SocketAddrV4::new(
                                    ipaddr,
                                    sockaddr.sin_port.to_be(), // swap byteorder
                                )));
                            Ok(())
                        }
                        (28, AbstractTcpSocketInner::Undefined6) => {
                            let sockaddr = WasmPtr::<SockaddrIn6>::new(sockaddr_ptr.offset())
                                .deref(memory)?
                                .get();
                            let ipaddr = Ipv6Addr::from(sockaddr.sin6_addr);
                            *sock_inner =
                                AbstractTcpSocketInner::Binded(SocketAddr::V6(SocketAddrV6::new(
                                    ipaddr,
                                    sockaddr.sin6_port.to_be(), // swap byteorder
                                    sockaddr.sin6_flowinfo,
                                    sockaddr.sin6_scope_id,
                                )));
                            Ok(())
                        }
                        _ => Err(__WASI_EINVAL),
                    }
                })
            }
            SyncOperation::SocketListen { fs, fd } => {
                let sock: &AbstractTcpSocket = fs.get_wasi_file_as::<AbstractTcpSocket>(fd)?;
                self.runtime.handle().block_on(async {
                    let mut sock_inner = sock.inner.write().await;
                    match &*sock_inner {
                        AbstractTcpSocketInner::Binded(addr) => {
                            let l = TcpListener::bind(addr).map_err(from_tokio_error).await?;
                            let fd = l.as_raw_fd();
                            let md = SocketMetadata {
                                fd,
                                local_address: l.local_addr().map_err(from_tokio_error)?,
                                remote_address: None,
                            };
                            *sock_inner = AbstractTcpSocketInner::Listening(AsyncMutex::new(l), md);
                            Ok(())
                        }
                        _ => Err(__WASI_EINVAL),
                    }
                })
            }
            SyncOperation::SocketAccept { memory, fs, fd_out, sockaddr_ptr, sockaddr_size } => {
                match self.accepted_rx.borrow_mut().try_recv() {
                    Ok((stream, addr)) => {
                        let fd_out_cell = fd_out.deref(memory)?;
                        let fd = stream.as_raw_fd();
                        let md = SocketMetadata {
                            fd,
                            local_address: stream.local_addr().map_err(from_tokio_error)?,
                            remote_address: Some(addr),
                        };
                        let (r, w) = stream.into_split();
                        let socket = AbstractTcpSocket {
                            inner: Arc::new(AsyncRwLock::new(AbstractTcpSocketInner::Stream(
                                AsyncMutex::new(r),
                                AsyncMutex::new(w),
                                md,
                            ))),
                            flags: 0,
                        };
                        let file = Box::new(socket);
                        let socket_name = format!("<accept:{}>", uuid::Uuid::new_v4());
                        let fd = fs
                            .open_file_at(
                                VIRTUAL_ROOT_FD,
                                file,
                                Fd::READ | Fd::WRITE,
                                socket_name,
                                ALL_RIGHTS,
                                ALL_RIGHTS,
                                0,
                            )
                            .map_err(|_| __WASI_EINVAL)?;
                        fd_out_cell.set(fd);
                        encode_socket_addr(memory, sockaddr_ptr, sockaddr_size, addr)?;
                        Ok(())
                    }
                    Err(e) => Err(__WASI_EAGAIN),
                }
            }
            SyncOperation::SocketAddr { memory, fs, fd, sockaddr_ptr, sockaddr_size_ptr, remote } => {
                let sock: &AbstractTcpSocket = fs.get_wasi_file_as::<AbstractTcpSocket>(fd)?;
                let sockaddr_size_ptr = sockaddr_size_ptr.deref(memory)?;
                let sockaddr_size = sockaddr_size_ptr.get();
                self.runtime.handle().block_on(async {
                    let sock_inner = sock.inner.read().await;
                    let actual_len: u32;
                    match &*sock_inner {
                        AbstractTcpSocketInner::Binded(addr) => {
                            if remote {
                                return Err(__WASI_ENOTCONN);
                            } else {
                                actual_len = encode_socket_addr(memory, sockaddr_ptr, sockaddr_size, *addr)?;
                            }
                        }
                        AbstractTcpSocketInner::Listening(_, ref md) =>{
                            if remote {
                                return Err(__WASI_ENOTCONN);
                            } else {
                                actual_len = encode_socket_addr(memory, sockaddr_ptr, sockaddr_size, md.local_address)?;
                            }
                        }
                        AbstractTcpSocketInner::Stream(_, _, ref md) => {
                            if remote {
                                match md.remote_address {
                                    Some(ref x) => {
                                        actual_len = encode_socket_addr(memory, sockaddr_ptr, sockaddr_size, *x)?;
                                    }
                                    None => {
                                        return Err(__WASI_ENOTCONN);
                                    }
                                }
                            } else {
                                actual_len = encode_socket_addr(memory, sockaddr_ptr, sockaddr_size, md.local_address)?;
                            }
                        }
                        _ => return Err(__WASI_EINVAL),
                    }
                    sockaddr_size_ptr.set(actual_len);
                    Ok(())
                })
            }
        }
    }

    fn wait(&self) -> Result<(__wasi_errno_t, UserContext), __wasi_errno_t> {
        if let Some(x) = self.local_completion.borrow_mut().pop_front() {
            Ok(x)
        } else {
            // TX and RX are in the same struct, so this should never fail.
            Ok(self.state.completed_rx.recv().unwrap())
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug, Clone)]
struct AbstractTcpSocket {
    inner: Arc<AsyncRwLock<AbstractTcpSocketInner>>,
    flags: __wasi_fdflags_t,
}

#[derive(Debug)]
enum AbstractTcpSocketInner {
    /// Created as an IPv4 socket, but is neither binded nor connected.
    Undefined4,

    /// Created as an IPv6 socket, but is neither binded nor connected.
    Undefined6,

    /// "Virtually" binded to a socket address.
    Binded(SocketAddr),

    /// Listening for incoming connections.
    Listening(AsyncMutex<TcpListener>, SocketMetadata),

    /// A connection is established on this socket.
    Stream(AsyncMutex<OwnedReadHalf>, AsyncMutex<OwnedWriteHalf>, SocketMetadata),
}

#[derive(Clone, Debug)]
struct SocketMetadata {
    fd: RawFd,
    local_address: SocketAddr,
    remote_address: Option<SocketAddr>,
}

impl AbstractTcpSocket {
    async fn run_io_maybe_nb<T>(&self, f: impl Future<Output = io::Result<T>>) -> io::Result<T> {
        // Disable NB mode since Tokio sometimes fails to run the first future to completion even if there is data immediately.
        // Caused by locks?
        if false && self.flags & __WASI_FDFLAG_NONBLOCK != 0 {
            tokio::select! {
                x = f => {
                    x
                }
                _ = async {} => {
                    Err(io::Error::from(io::ErrorKind::WouldBlock))
                }
            }
        } else {
            f.await
        }
    }
}

impl Seek for AbstractTcpSocket {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        Err(io::Error::from(io::ErrorKind::Other)) // seek operation not supported on sockets
    }
}

impl Read for AbstractTcpSocket {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        executor::with_current::<TokioExecutor, _, _>(|current| {
            let current = match current {
                Some(x) => x,
                None => return Err(io::Error::from(io::ErrorKind::WouldBlock))
            };
            current.runtime.handle().block_on(async {
                let sock_inner = self.inner.read().await;
                match *sock_inner {
                    AbstractTcpSocketInner::Stream(ref read_half, _, _) => {
                        let mut read_half = read_half.lock().await;
                        self.run_io_maybe_nb(read_half.read(data)).await
                    }
                    _ => Err(io::Error::from(io::ErrorKind::BrokenPipe)),
                }
            })
        })
    }
}

impl Write for AbstractTcpSocket {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        executor::with_current::<TokioExecutor, _, _>(|current| {
            let current = match current {
                Some(x) => x,
                None => return Err(io::Error::from(io::ErrorKind::WouldBlock))
            };
            current.runtime.handle().block_on(async {
                let sock_inner = self.inner.read().await;
                match *sock_inner {
                    AbstractTcpSocketInner::Stream(_, ref write_half, _) => {
                        let mut write_half = write_half.lock().await;
                        self.run_io_maybe_nb(write_half.write(&data)).await
                    }
                    _ => Err(io::Error::from(io::ErrorKind::BrokenPipe)),
                }
            })
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl serde::Serialize for AbstractTcpSocket {
    fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        unimplemented!()
    }
}

impl<'de> serde::Deserialize<'de> for AbstractTcpSocket {
    fn deserialize<D: serde::Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
        unimplemented!()
    }
}

#[typetag::serde]
impl WasiFile for AbstractTcpSocket {
    fn last_accessed(&self) -> u64 {
        0
    }
    fn last_modified(&self) -> u64 {
        0
    }
    fn created_time(&self) -> u64 {
        0
    }
    fn size(&self) -> u64 {
        0
    }
    fn set_len(&mut self, _new_size: __wasi_filesize_t) -> Result<(), WasiFsError> {
        Err(WasiFsError::PermissionDenied)
    }
    fn unlink(&mut self) -> Result<(), WasiFsError> {
        Ok(())
    }

    fn bytes_available(&self) -> Result<usize, WasiFsError> {
        Ok(0)
    }

    #[cfg(unix)]
    fn get_raw_fd(&self) -> Option<i32> {
        use std::os::unix::io::AsRawFd;

        executor::with_current::<TokioExecutor, _, _>(|current| {
            let current = match current {
                Some(x) => x,
                None => return None
            };
            current.runtime.handle().block_on(async {
                let sock_inner = self.inner.read().await;
                match *sock_inner {
                    AbstractTcpSocketInner::Listening(_, ref md) => Some(md.fd),
                    AbstractTcpSocketInner::Stream(_, _, ref md) => Some(md.fd),
                    _ => None,
                }
            })
        })
    }

    #[cfg(not(unix))]
    fn get_raw_fd(&self) -> Option<i32> {
        unimplemented!(
            "AbstractTcpSocket::get_raw_fd in WasiFile is not implemented for non-Unix-like targets yet"
        );
    }

    fn try_clone_dyn(&self) -> Option<Box<dyn WasiFile>> {
        Some(Box::new(self.clone()))
    }

    fn update_flags(&mut self, flags: __wasi_fdflags_t) -> Result<(), WasiFsError> {
        self.flags = flags;
        Ok(())
    }

    // TODO: Fix this and move into a better abstraction structure
    fn is_socket(&self) -> bool {
        true
    }
}

fn from_tokio_error(e: tokio::io::Error) -> __wasi_errno_t {
    use tokio::io::ErrorKind;
    match e.kind() {
        ErrorKind::NotFound => __WASI_EEXIST,
        ErrorKind::PermissionDenied => __WASI_EPERM,
        ErrorKind::ConnectionRefused => __WASI_ECONNREFUSED,
        ErrorKind::ConnectionReset => __WASI_ECONNRESET,
        ErrorKind::ConnectionAborted => __WASI_ECONNABORTED,
        ErrorKind::NotConnected => __WASI_ENOTCONN,
        ErrorKind::AddrInUse => __WASI_EADDRINUSE,
        ErrorKind::AddrNotAvailable => __WASI_EADDRNOTAVAIL,
        ErrorKind::BrokenPipe => __WASI_EPIPE,
        ErrorKind::AlreadyExists => __WASI_EEXIST,
        ErrorKind::WouldBlock => __WASI_EAGAIN,
        ErrorKind::InvalidInput => __WASI_EINVAL,
        ErrorKind::InvalidData => __WASI_EINVAL,
        ErrorKind::TimedOut => __WASI_ETIMEDOUT,
        ErrorKind::WriteZero => __WASI_EINVAL,
        ErrorKind::Interrupted => __WASI_EAGAIN,
        ErrorKind::UnexpectedEof => __WASI_EIO,
        _ => __WASI_EINVAL,
    }
}

fn decode_socket_addr(memory: &Memory, sockaddr_ptr: WasmPtr<u8, Array>, sockaddr_size: u32) -> Result<SocketAddr, __wasi_errno_t> {
    match sockaddr_size {
        16 => {
            let sockaddr = WasmPtr::<SockaddrIn>::new(sockaddr_ptr.offset())
                .deref(memory)?
                .get();
            let ipaddr = Ipv4Addr::from(sockaddr.sin_addr);
            Ok(SocketAddr::V4(SocketAddrV4::new(
                ipaddr,
                sockaddr.sin_port.to_be(), // swap byteorder
            )))
        }
        28 => {
            let sockaddr = WasmPtr::<SockaddrIn6>::new(sockaddr_ptr.offset())
                .deref(memory)?
                .get();
            let ipaddr = Ipv6Addr::from(sockaddr.sin6_addr);
            Ok(SocketAddr::V6(SocketAddrV6::new(
                ipaddr,
                sockaddr.sin6_port.to_be(), // swap byteorder
                sockaddr.sin6_flowinfo,
                sockaddr.sin6_scope_id,
            )))

        }
        _ => Err(__WASI_EINVAL)
    }
}

fn encode_socket_addr(memory: &Memory, sockaddr_ptr: WasmPtr<u8, Array>, sockaddr_size: u32, addr: SocketAddr) -> Result<u32, __wasi_errno_t> {
    match addr {
        SocketAddr::V4(addr) => {
            if sockaddr_size < 16 {
                return Err(__WASI_EINVAL);
            }
            let target = WasmPtr::<SockaddrIn>::new(sockaddr_ptr.offset()).deref(memory)?;
            target.set(SockaddrIn {
                sin_family: AF_INET as _,
                sin_port: addr.port().to_be(),
                sin_addr: addr.ip().octets(),
                sin_zero: [0; 8],
            });
            Ok(16)
        }
        SocketAddr::V6(addr) => {
            if sockaddr_size < 28 {
                return Err(__WASI_EINVAL);
            }
            let target = WasmPtr::<SockaddrIn6>::new(sockaddr_ptr.offset()).deref(memory)?;
            target.set(SockaddrIn6 {
                sin6_family: AF_INET6 as _,
                sin6_port: addr.port().to_be(),
                sin6_flowinfo: 0,
                sin6_addr: addr.ip().octets(),
                sin6_scope_id: 0,
            });
            Ok(28)
        }
    }
}