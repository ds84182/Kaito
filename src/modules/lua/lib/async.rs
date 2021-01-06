use anyhow::Result;
use crossbeam::channel::Sender;
use mlua::{
    prelude::{LuaError, LuaMultiValue},
    Function, Lua, RegistryKey, Table,
};
use std::time::Duration;

use super::super::state::{LuaAsyncCallback, SandboxState};

fn create_future(state: &Lua) -> Result<(RegistryKey, Table)> {
    let async_tbl: Table = state.globals().get("async")?;
    let fut_fn: Function = async_tbl.get("__RustFuture")?;

    let fut: Table = fut_fn.call(())?;
    let fut_reg_key = state.create_registry_value(fut.clone())?;

    Ok((fut_reg_key, fut))
}

macro_rules! wrap_future {
    ($state:expr, $fut:expr) => {
        match $fut {
            Ok(a) => a,
            Err(err) => return Err(LuaError::RuntimeError(err.to_string())),
        }
    };
}

pub fn lib_async(state: &Lua, sender: Sender<LuaAsyncCallback>) -> Result<()> {
    let async_tbl = state.create_table()?;

    // async.delay
    let async_delay = state.create_function(move |state, duration: f64| {
        let (future_reg_key, fut) = wrap_future!(state, create_future(state));

        if duration.is_sign_negative() || !duration.is_finite() {
            return Err(LuaError::RuntimeError("Invalid duration".into()));
        }

        let duration = Duration::from_secs_f64(duration);

        let sandbox_state = state.named_registry_value("__SANDBOX_STATE").ok().clone();

        let sender = sender.clone();
        tokio::spawn(async move {
            tokio::time::delay_for(duration).await;

            sender
                .send((
                    future_reg_key,
                    sandbox_state,
                    Box::new(|_state| LuaMultiValue::new()),
                ))
                .unwrap();
        });

        Ok(fut)
    })?;
    async_tbl.set("delay", async_delay)?;

    state.globals().set("async", async_tbl)?;

    Ok(())
}
