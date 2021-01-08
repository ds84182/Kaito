use anyhow::Result;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use thiserror::Error;

use crate::services::{ChannelId, ServerId};

macro_rules! settings {
    ($sname:ident, { $($name:ident: $type:ty => ($default:expr, $flags:expr, $help:expr, [ $($setting_ident:ident => $setting_value:expr)* ])),* }) => {
        pub struct $sname {
            $(
                pub $name: crate::settings::Setting<$type>,
            )*
        }

        impl $sname {
            pub fn create() -> Result<Arc<$sname>> {
                $(
                    #[allow(unused, non_camel_case_types)]
                    type $name = <$type as SettingValue>::Parameters;
                )*

                Ok(Arc::new($sname {
                    $(
                        $name: Setting::create(stringify!($name).into(), $default, $name {
                            $($setting_ident: Some($setting_value),)*
                            ..Default::default()
                        }, $flags, $help.into())?,
                    )*
                }))
            }
        }
    };
}

bitflags! {
    pub struct SettingFlags: u8 {
        const SERVER_OVERRIDE = 1;
    }
}

pub struct Setting<T>
where
    T: SettingValue + Send + Sync,
    T::Parameters: Send + Sync,
{
    name: String,
    flags: SettingFlags,
    help: String,
    parameters: T::Parameters,
    default: T,
    cached_channel_values: DashMap<ChannelId, T>,
    cached_server_values: DashMap<ServerId, T>,
}

impl<T> Setting<T>
where
    T: SettingValue + Send + Sync,
    T::Parameters: Send + Sync,
{
    pub fn create(
        name: &str,
        default: T,
        parameters: T::Parameters,
        flags: SettingFlags,
        help: String,
    ) -> Result<Setting<T>> {
        SettingValue::is_valid(&default, &parameters)?;

        Ok(Setting {
            name: name.to_string(),
            parameters: parameters,
            flags,
            help,
            default,
            cached_channel_values: DashMap::new(),
            cached_server_values: DashMap::new(),
        })
    }

    pub async fn value(&self, server_id: ServerId, channel_id: ChannelId) -> Result<T> {
        if self.flags.contains(SettingFlags::SERVER_OVERRIDE) {
            if let Some(value) = self.get_server_value(server_id).await? {
                return Ok(value);
            }

            if let Some(value) = self.get_channel_value(channel_id).await? {
                return Ok(value);
            }

            Ok(self.default.clone())
        } else {
            if let Some(value) = self.get_channel_value(channel_id).await? {
                return Ok(value);
            }

            if let Some(value) = self.get_server_value(server_id).await? {
                return Ok(value);
            }

            Ok(self.default.clone())
        }
    }

    pub fn flush_cache(&self) {
        self.cached_channel_values.clear();
        self.cached_server_values.clear();
    }

    async fn get_channel_value(&self, channel_id: ChannelId) -> Result<Option<T>> {
        if let Some(cached) = self
            .cached_channel_values
            .get(&channel_id)
            .map(|v| v.value().clone())
        {
            Ok(Some(cached))
        } else {
            // TODO: Read DB
            Ok(None)
        }
    }

    async fn get_server_value(&self, server_id: ServerId) -> Result<Option<T>> {
        if let Some(cached) = self
            .cached_server_values
            .get(&server_id)
            .map(|v| v.value().clone())
        {
            Ok(Some(cached))
        } else {
            // TODO: Read DB
            Ok(None)
        }
    }

    pub fn set_value(&self, ctx: SettingContext, input: &str) -> Result<()> {
        let value = T::set_value(input, &self.parameters)?;

        // TODO: Insert to DB

        match ctx {
            SettingContext::Channel(channel_id) => {
                self.cached_channel_values.insert(channel_id, value)
            }
            SettingContext::Server(server_id) => self.cached_server_values.insert(server_id, value),
        };

        Ok(())
    }
}

pub trait SettingValue: Clone + Sized + Deserialize<'static> + Serialize {
    type Parameters;

    // Let the value type check that the default value is valid based on the paramters
    fn is_valid(value: &Self, parameters: &Self::Parameters) -> Result<()>;
    // Set
    fn set_value(input: &str, parameters: &Self::Parameters) -> Result<Self>;
}

// Setting value - bool

impl SettingValue for bool {
    type Parameters = SettingBoolParameters;

    fn is_valid(_value: &bool, _parameters: &SettingBoolParameters) -> Result<()> {
        Ok(())
    }

    fn set_value(input: &str, _parameters: &SettingBoolParameters) -> Result<bool> {
        let trimmed = input.trim();

        if trimmed == "true" {
            return Ok(true);
        } else if trimmed == "false" {
            return Ok(false);
        }

        if let Some(val) = u32::from_str(trimmed).ok() {
            return Ok(val > 0);
        }

        Err(SettingError::UnexpectedInput {
            expected: SettingType::Bool,
            input: input.into(),
        }
        .into())
    }
}

#[derive(Default)]
pub struct SettingBoolParameters {}

// Setting value - String

impl SettingValue for String {
    type Parameters = SettingStringParameters;

    fn is_valid(value: &String, parameters: &SettingStringParameters) -> Result<()> {
        if let Some(max_len) = parameters.max_len {
            let len = value.len();
            if len > max_len {
                return Err(SettingError::ExceededMaxLength {
                    max: max_len,
                    length: len,
                }
                .into());
            }
        }

        Ok(())
    }

    fn set_value(input: &str, parameters: &SettingStringParameters) -> Result<String> {
        <String as SettingValue>::is_valid(&input.into(), parameters)?;

        Ok(input.into())
    }
}

#[derive(Default)]
pub struct SettingStringParameters {
    pub max_len: Option<usize>,
}

pub enum SettingContext {
    Channel(ChannelId),
    Server(ServerId),
}

#[derive(Debug, Copy, Clone)]
pub enum SettingType {
    Bool,
}

#[derive(Debug, Error)]
pub enum SettingError {
    #[error("unable to parse \"{}\" as {:?}", input, expected)]
    UnexpectedInput {
        expected: SettingType,
        input: String,
    },
    #[error("len {} exceeded max length {}", length, max)]
    ExceededMaxLength { max: usize, length: usize },
}

pub mod prelude {
    pub use super::{Setting, SettingBoolParameters, SettingFlags, SettingValue};
}