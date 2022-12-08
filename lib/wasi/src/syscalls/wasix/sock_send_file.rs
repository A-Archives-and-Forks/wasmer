use wasmer_vfs::AsyncReadExt;

use super::*;
use crate::syscalls::*;

/// ### `sock_send_file()`
/// Sends the entire contents of a file down a socket
///
/// ## Parameters
///
/// * `in_fd` - Open file that has the data to be transmitted
/// * `offset` - Offset into the file to start reading at
/// * `count` - Number of bytes to be sent
///
/// ## Return
///
/// Number of bytes transmitted.
pub fn sock_send_file<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    sock: WasiFd,
    in_fd: WasiFd,
    offset: Filesize,
    mut count: Filesize,
    ret_sent: WasmPtr<Filesize, M>,
) -> Result<Errno, WasiError> {
    debug!(
        "wasi[{}:{}]::send_file (fd={}, file_fd={})",
        ctx.data().pid(),
        ctx.data().tid(),
        sock,
        in_fd
    );

    wasi_try_ok!(ctx.data().clone().process_signals_and_exit(&mut ctx)?);

    let mut env = ctx.data();
    let net = env.net();
    let tasks = env.tasks.clone();
    let state = env.state.clone();

    let ret = wasi_try_ok!({
        // Set the offset of the file
        {
            let mut fd_map = state.fs.fd_map.write().unwrap();
            let fd_entry = wasi_try_ok!(fd_map.get_mut(&in_fd).ok_or(Errno::Badf));
            fd_entry.offset.store(offset as u64, Ordering::Release);
        }

        // Enter a loop that will process all the data
        let mut total_written: Filesize = 0;
        while (count > 0) {
            let sub_count = count.min(4096);
            count -= sub_count;

            let fd_entry = wasi_try_ok!(state.fs.get_fd(in_fd));
            let data = {
                let inodes = env.state.inodes.clone();
                match in_fd {
                    __WASI_STDIN_FILENO => {
                        let inodes = inodes.read().unwrap();
                        let mut stdin = wasi_try_ok!(inodes
                            .stdin_mut(&state.fs.fd_map)
                            .map_err(fs_error_into_wasi_err));
                        let data = wasi_try_ok!(__asyncify(&mut ctx, None, async move {
                            let mut buf = Vec::with_capacity(sub_count as usize);
                            unsafe {
                                buf.set_len(sub_count as usize);
                            }
                            let amt = stdin.read(&mut buf[..]).await.map_err(map_io_err)?;
                            unsafe {
                                buf.set_len(amt);
                            }
                            Ok(buf)
                        })?);
                        env = ctx.data();
                        data
                    }
                    __WASI_STDOUT_FILENO | __WASI_STDERR_FILENO => return Ok(Errno::Inval),
                    _ => {
                        if !fd_entry.rights.contains(Rights::FD_READ) {
                            // TODO: figure out the error to return when lacking rights
                            return Ok(Errno::Access);
                        }

                        let offset = fd_entry.offset.load(Ordering::Acquire) as usize;
                        let inode_idx = fd_entry.inode;
                        let inodes = inodes.read().unwrap();
                        let inode = &inodes.arena[inode_idx];

                        let data = {
                            let mut guard = inode.write();
                            match guard.deref_mut() {
                                Kind::File { handle, .. } => {
                                    if let Some(handle) = handle {
                                        let data =
                                            wasi_try_ok!(__asyncify(&mut ctx, None, async move {
                                                let mut buf =
                                                    Vec::with_capacity(sub_count as usize);
                                                unsafe {
                                                    buf.set_len(sub_count as usize);
                                                }

                                                let mut handle = handle.write().unwrap();
                                                handle
                                                    .seek(std::io::SeekFrom::Start(offset as u64))
                                                    .await
                                                    .map_err(map_io_err)?;
                                                let amt = handle
                                                    .read(&mut buf[..])
                                                    .await
                                                    .map_err(map_io_err)?;
                                                unsafe {
                                                    buf.set_len(amt);
                                                }
                                                Ok(buf)
                                            })?);
                                        env = ctx.data();
                                        data
                                    } else {
                                        return Ok(Errno::Inval);
                                    }
                                }
                                Kind::Socket { socket } => {
                                    let socket = socket.clone();
                                    let tasks = tasks.clone();
                                    drop(guard);
                                    drop(inodes);

                                    let data =
                                        wasi_try_ok!(__asyncify(&mut ctx, None, async move {
                                            socket
                                                .recv(sub_count as usize)
                                                .await
                                                .map(|a| a.to_vec())
                                        })?);
                                    env = ctx.data();
                                    data
                                }
                                Kind::Pipe { ref mut pipe } => {
                                    let data =
                                        wasi_try_ok!(__asyncify(&mut ctx, None, async move {
                                            let mut buf = Vec::with_capacity(sub_count as usize);
                                            unsafe {
                                                buf.set_len(sub_count as usize);
                                            }

                                            let amt =
                                                wasmer_vfs::AsyncReadExt::read(pipe, &mut buf[..])
                                                    .await
                                                    .map_err(map_io_err)?;
                                            unsafe {
                                                buf.set_len(amt);
                                            }
                                            Ok(buf)
                                        })?);
                                    env = ctx.data();
                                    data
                                }
                                Kind::Dir { .. } | Kind::Root { .. } => {
                                    return Ok(Errno::Isdir);
                                }
                                Kind::EventNotifications { .. } => {
                                    return Ok(Errno::Inval);
                                }
                                Kind::Symlink { .. } => unimplemented!("Symlinks in wasi::fd_read"),
                                Kind::Buffer { buffer } => {
                                    let mut buf = Vec::with_capacity(sub_count as usize);
                                    unsafe {
                                        buf.set_len(sub_count as usize);
                                    }

                                    let mut buf_read = &buffer[offset..];
                                    let amt = wasi_try_ok!(std::io::Read::read(
                                        &mut buf_read,
                                        &mut buf[..]
                                    )
                                    .map_err(map_io_err));
                                    unsafe {
                                        buf.set_len(amt);
                                    }
                                    buf
                                }
                            }
                        };

                        // reborrow
                        let mut fd_map = state.fs.fd_map.write().unwrap();
                        let fd_entry = wasi_try_ok!(fd_map.get_mut(&in_fd).ok_or(Errno::Badf));
                        fd_entry
                            .offset
                            .fetch_add(data.len() as u64, Ordering::AcqRel);

                        data
                    }
                }
            };

            // Write it down to the socket
            let bytes_written = wasi_try_ok!(__sock_actor_mut(
                &mut ctx,
                sock,
                Rights::SOCK_SEND,
                move |socket| async move { socket.send(data).await },
            )?);
            env = ctx.data();

            total_written += bytes_written as u64;
        }

        let memory = env.memory_view(&ctx);
        wasi_try_mem_ok!(ret_sent.write(&memory, total_written as Filesize));

        Ok(Errno::Success)
    });
    Ok(ret)
}
