use anyhow::Result;
use crossbeam::channel::{unbounded, Receiver, Sender};
use governor::{
    clock::QuantaClock,
    state::{direct::NotKeyed, InMemoryState},
    Quota, RateLimiter,
};
use mlua::{
    prelude::{LuaError, LuaMultiValue, LuaValue},
    Function, Lua, RegistryKey, StdLib, Table, ToLua, UserData, UserDataMethods,
};
use paste::paste;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use super::{
    http,
    lib::{
        bot::{lib_bot, BotMessage},
        include_lua, lib_include,
        os::lib_os,
        r#async::lib_async,
    },
};
use crate::bot::Bot;

pub type LuaAsyncCallback = (
    RegistryKey,
    Option<SandboxState>,
    Box<dyn Fn(&Lua) -> Result<LuaMultiValue, String> + Send>,
);

macro_rules! atomic_get_set {
    ($ident:ident, $ty:ty) => {
        paste! {
            pub fn[<set_ $ident>](&self, value: $ty) {
                self.$ident.store(value, Ordering::Relaxed)
            }

            pub fn $ident(&self) -> $ty {
                self.$ident.load(Ordering::Relaxed)
            }
        }
    };
}

pub struct LuaState {
    inner: Lua,
    sandbox: bool,
    async_sender: Sender<LuaAsyncCallback>,
    async_receiver: Receiver<LuaAsyncCallback>,
    http_rate_limiter: Arc<RateLimiter<NotKeyed, InMemoryState, QuantaClock>>,
}

impl LuaState {
    pub fn create_state(bot: &Arc<Bot>, sandbox: bool) -> Result<LuaState> {
        // Avoid loading os and io
        let inner = unsafe {
            Lua::unsafe_new_with(
                StdLib::COROUTINE
                    | StdLib::TABLE
                    | StdLib::STRING
                    | StdLib::UTF8
                    | StdLib::MATH
                    | StdLib::DEBUG,
            )
        };

        let (async_sender, async_receiver) = unbounded();

        lib_async(&inner, async_sender.clone())?;
        lib_os(&inner)?;

        let lua_root_path = bot.root_path().join("lua");

        lib_include(lua_root_path.clone(), &inner)?;

        if sandbox {
            include_lua(&inner, &lua_root_path, "sandbox.lua")?;
        } else {
            lib_bot(&inner)?;
            include_lua(&inner, &lua_root_path, "bot.lua")?;
        }

        // Limit memory to 256 MiB
        inner.set_memory_limit(256 * 1024 * 1024)?;

        let http_rate_limiter = Arc::new(RateLimiter::direct(Quota::per_second(
            std::num::NonZeroU32::new(2).unwrap(),
        )));

        Ok(LuaState {
            inner,
            sandbox,
            async_sender,
            async_receiver,
            http_rate_limiter,
        })
    }

    pub fn run_bot_command(&self, msg: BotMessage, args: Vec<String>) -> Result<()> {
        let sandbox_tbl: Table = self.inner.globals().get("bot")?;
        let on_command_fn: Function = sandbox_tbl.get("on_command")?;

        on_command_fn.call((msg, args))?;

        Ok(())
    }

    pub fn run_sandboxed(
        &self,
        source: &str,
    ) -> Result<(Arc<SandboxStateInner>, Receiver<SandboxMsg>)> {
        let sandbox_tbl: Table = self.inner.globals().get("sandbox")?;
        let run_fn: Function = sandbox_tbl.get("run")?;

        let (sender, receiver) = unbounded();

        let sandbox_state = SandboxState(Arc::new(SandboxStateInner {
            async_sender: self.async_sender.clone(),
            sender: sender.clone(),
            instructions_run: AtomicU64::new(0),
            limits: SandboxLimits {
                lines_left: AtomicU64::new(10),
                characters_left: AtomicU64::new(2000),
                http_calls_left: AtomicU64::new(2),
            },
            http_rate_limiter: self.http_rate_limiter.clone(),
        }));

        self.inner
            .set_named_registry_value("__SANDBOX_STATE", sandbox_state.clone())?;

        run_fn.call((sandbox_state.clone(), source))?;

        Ok((sandbox_state.0, receiver))
    }

    pub fn think(&self) -> Result<()> {
        if self.sandbox {
            let sandbox_tbl: Table = self.inner.globals().get("sandbox")?;
            let think_fn: Function = sandbox_tbl.get("think")?;
            think_fn.call(())?;
        } else {
            let bot_tbl: Table = self.inner.globals().get("bot")?;
            let think_fn: Function = bot_tbl.get("think")?;
            think_fn.call(())?;
        }

        loop {
            // Check for async callbacks
            match self.async_receiver.try_recv() {
                Ok((fut_reg_key, sandbox_state, cb)) => {
                    let (succ, value) = match cb(&self.inner) {
                        Ok(vals) => (true, vals),
                        Err(err) => (
                            false,
                            LuaMultiValue::from_vec(vec![LuaValue::String(
                                self.inner.create_string(&err)?,
                            )]),
                        ),
                    };
                    let future: Table = self.inner.registry_value(&fut_reg_key)?;
                    let resolve_fn: Function = if succ {
                        future.get("__handle_resolve")?
                    } else {
                        future.get("__handle_reject")?
                    };

                    // Sandbox when resolving the future
                    if self.sandbox {
                        if let Some(sandbox_state) = sandbox_state {
                            let sandbox_tbl: Table = self.inner.globals().get("sandbox")?;
                            let run_fn: Function = sandbox_tbl.get("async_callback")?;

                            let args = LuaMultiValue::from_vec(
                                [
                                    vec![
                                        sandbox_state.to_lua(&self.inner)?,
                                        LuaValue::Table(future.clone()),
                                        LuaValue::Boolean(true),
                                    ],
                                    value.into_vec(),
                                ]
                                .concat(),
                            );

                            run_fn.call::<_, ()>(args)?;
                        }
                    } else {
                        let args = LuaMultiValue::from_vec(
                            [
                                vec![LuaValue::Table(future.clone()), LuaValue::Boolean(true)],
                                value.into_vec(),
                            ]
                            .concat(),
                        );

                        resolve_fn.call::<_, ()>(args)?;
                    }

                    // Clean up the async registry values
                    self.inner.remove_registry_value(fut_reg_key)?;
                }
                _ => break,
            }
        }

        Ok(())
    }
}

pub enum SandboxMsg {
    Out(String),
    Error(String),
    Terminated(SandboxTerminationReason),
}

pub enum SandboxTerminationReason {
    ExecutionQuota,
}

#[derive(Clone)]
pub struct SandboxState(pub Arc<SandboxStateInner>);

pub struct SandboxStateInner {
    pub async_sender: Sender<LuaAsyncCallback>,
    pub sender: Sender<SandboxMsg>,
    pub instructions_run: AtomicU64,
    pub limits: SandboxLimits,
    pub http_rate_limiter: Arc<RateLimiter<NotKeyed, InMemoryState, QuantaClock>>,
}

pub struct SandboxLimits {
    pub lines_left: AtomicU64,
    pub characters_left: AtomicU64,
    pub http_calls_left: AtomicU64,
}

impl SandboxLimits {
    atomic_get_set! {lines_left, u64}
    atomic_get_set! {characters_left, u64}
}

impl UserData for SandboxState {
    fn add_methods<'a, M: UserDataMethods<'a, Self>>(methods: &mut M) {
        methods.add_method("print", |_, this, value: String| {
            this.0.sender.send(SandboxMsg::Out(value)).ok(); // Ignore the error for now
            Ok(())
        });

        methods.add_method("error", |_, this, value: String| {
            this.0.sender.send(SandboxMsg::Error(value)).ok(); // Ignore the error for now
            Ok(())
        });

        methods.add_method("set_instructions_run", |_, this, value: u64| {
            this.0.instructions_run.store(value, Ordering::Relaxed);
            Ok(())
        });

        methods.add_method("get_instructions_run", |_, this, _: ()| {
            Ok(this.0.instructions_run.load(Ordering::Relaxed))
        });

        methods.add_method("set_state", |state, this, _: ()| {
            state.set_named_registry_value("__SANDBOX_STATE", this.clone())?;
            Ok(())
        });

        methods.add_method(
            "http_fetch",
            |state, this, (url, options): (String, Table)| {
                http::http_fetch(state, this, &url, options)
            },
        );

        methods.add_method("terminate", |_, this, value: String| {
            let reason = match value.as_ref() {
                "exec" => SandboxTerminationReason::ExecutionQuota,
                _ => {
                    return Err(LuaError::RuntimeError(format!(
                        "unknown termination reason: \"{}\"",
                        value
                    )))
                }
            };

            this.0.sender.send(SandboxMsg::Terminated(reason)).ok();

            Ok(())
        });
    }
}