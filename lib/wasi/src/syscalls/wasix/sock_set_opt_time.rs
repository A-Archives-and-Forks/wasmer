use super::*;
use crate::syscalls::*;

/// ### `sock_set_opt_time()`
/// Sets one of the times the socket
///
/// ## Parameters
///
/// * `fd` - Socket descriptor
/// * `sockopt` - Socket option to be set
/// * `time` - Value to set the time to
pub fn sock_set_opt_time<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: WasiFd,
    opt: Sockoption,
    time: WasmPtr<OptionTimestamp, M>,
) -> Result<Errno, WasiError> {
    debug!(
        "wasi[{}:{}]::sock_set_opt_time(fd={}, ty={})",
        ctx.data().pid(),
        ctx.data().tid(),
        sock,
        opt
    );

    let env = ctx.data();
    let memory = env.memory_view(&ctx);
    let time = wasi_try_mem_ok!(time.read(&memory));
    let time = match time.tag {
        OptionTag::None => None,
        OptionTag::Some => Some(Duration::from_nanos(time.u)),
        _ => return Ok(Errno::Inval),
    };

    let ty = match opt {
        Sockoption::RecvTimeout => wasmer_vnet::TimeType::ReadTimeout,
        Sockoption::SendTimeout => wasmer_vnet::TimeType::WriteTimeout,
        Sockoption::ConnectTimeout => wasmer_vnet::TimeType::ConnectTimeout,
        Sockoption::AcceptTimeout => wasmer_vnet::TimeType::AcceptTimeout,
        Sockoption::Linger => wasmer_vnet::TimeType::Linger,
        _ => return Ok(Errno::Inval),
    };

    let option: crate::net::socket::WasiSocketOption = opt.into();
    wasi_try_ok!(__sock_actor_mut(
        &mut ctx,
        sock,
        Rights::empty(),
        move |socket| async move { socket.set_opt_time(ty, time) }
    )?);
    Ok(Errno::Success)
}
