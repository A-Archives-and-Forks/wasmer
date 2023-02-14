pub mod builtins;

use std::{collections::HashMap, sync::Arc};

use crate::{
    vbus::{BusSpawnedProcess, VirtualBusError},
    WasiRuntimeError,
};
use wasmer::{FunctionEnvMut, Store};
use wasmer_wasi_types::wasi::Errno;

use crate::{syscalls::stderr_write, WasiEnv};

/// A command available to an OS environment.
pub trait VirtualCommand
where
    Self: std::fmt::Debug,
{
    /// Returns the canonical name of the command.
    fn name(&self) -> &str;

    /// Retrieve the command as as a [`std::any::Any`] reference.
    fn as_any(&self) -> &dyn std::any::Any;

    /// Executes the command.
    fn exec<'a>(
        &self,
        parent_ctx: &FunctionEnvMut<'a, WasiEnv>,
        path: &str,
        store: Store,
        config: WasiEnv,
    ) -> Result<BusSpawnedProcess, WasiRuntimeError>;
}

#[derive(Debug, Clone)]
pub struct Commands {
    commands: HashMap<String, Arc<dyn VirtualCommand + Send + Sync + 'static>>,
}

impl Commands {
    pub fn new() -> Self {
        Self {
            commands: HashMap::new(),
        }
    }

    /// Register a command.
    ///
    /// The command will be available with it's canonical name ([`VirtualCommand::name()`]) at /bin/NAME.
    pub fn register_command<C: VirtualCommand + Send + Sync + 'static>(&mut self, cmd: C) {
        let path = format!("/bin/{}", cmd.name());
        self.register_command_with_path(cmd, path);
    }

    /// Register a command at a custom path.
    pub fn register_command_with_path<C: VirtualCommand + Send + Sync + 'static>(
        &mut self,
        cmd: C,
        path: String,
    ) {
        self.commands.insert(path, Arc::new(cmd));
    }

    /// Determine if a command exists at the given path.
    pub fn exists(&self, path: &str) -> bool {
        let name = path.to_string();
        self.commands.contains_key(&name)
    }

    /// Get a command by its path.
    pub fn get(&self, path: &str) -> Option<&Arc<dyn VirtualCommand + Send + Sync + 'static>> {
        self.commands.get(path)
    }

    /// Execute a command.
    pub fn exec<'a>(
        &self,
        parent_ctx: &FunctionEnvMut<'a, WasiEnv>,
        path: &str,
        store: Store,
        builder: WasiEnv,
    ) -> Result<BusSpawnedProcess, WasiRuntimeError> {
        let path = path.to_string();
        if let Some(cmd) = self.commands.get(&path) {
            cmd.exec(parent_ctx, path.as_str(), store, builder)
        } else {
            let _ = stderr_write(
                parent_ctx,
                format!("wasm command unknown - {}\r\n", path).as_bytes(),
            );
            Ok(BusSpawnedProcess::exited_process(Errno::Noent as u32))
        }
    }
}
