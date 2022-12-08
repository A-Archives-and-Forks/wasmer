use super::*;
use crate::syscalls::*;

/// ### `sock_listen()`
/// Listen for connections on a socket
///
/// Polling the socket handle will wait until a connection
/// attempt is made
///
/// Note: This is similar to `listen`
///
/// ## Parameters
///
/// * `fd` - File descriptor of the socket to be bind
/// * `backlog` - Maximum size of the queue for pending connections
pub fn sock_listen<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: WasiFd,
    backlog: M::Offset,
) -> Result<Errno, WasiError> {
    debug!(
        "wasi[{}:{}]::sock_listen (fd={})",
        ctx.data().pid(),
        ctx.data().tid(),
        sock
    );

    let env = ctx.data();
    let net = env.net();
    let backlog: usize = wasi_try_ok!(backlog.try_into().map_err(|_| Errno::Inval));
    wasi_try_ok!(__sock_upgrade(
        &mut ctx,
        sock,
        Rights::SOCK_LISTEN,
        move |socket| async move { socket.listen(net, backlog).await }
    )?);
    Ok(Errno::Success)
}
