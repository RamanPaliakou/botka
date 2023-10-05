use std::marker::PhantomData;

use diesel::result::Error::DeserializationError;
use diesel::{
    ExpressionMethods, OptionalExtension, QueryDsl, RunQueryDsl,
    SqliteConnection,
};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::{models, schema};

/// A definition for a typed value stored in the database table `options`.
pub struct ConfigOptionDef<T: Serialize + DeserializeOwned> {
    key_name: &'static str,
    phantom: PhantomData<T>,
}

/// A helper macro for defining a `ConfigOptionDef` constant.
macro_rules! config_option_def {
    ($name:ident, $type:ty) => {
        #[allow(non_upper_case_globals)]
        pub const $name: crate::db::ConfigOptionDef<$type> =
            crate::db::ConfigOptionDef::new(stringify!($name));
    };
}
pub(crate) use config_option_def;

impl<T: Serialize + DeserializeOwned> ConfigOptionDef<T> {
    pub const fn new(key_name: &'static str) -> Self {
        Self { key_name, phantom: PhantomData }
    }

    /// Get the value of this option from the database.
    /// Returns `Ok(None)` if the option is not set or deserialization fails.
    pub fn get(
        &self,
        conn: &mut SqliteConnection,
    ) -> diesel::QueryResult<Option<T>> {
        let value: Option<String> = schema::options::table
            .filter(schema::options::name.eq(self.key_name))
            .first::<models::ConfigOption>(conn)
            .optional()?
            .map(|option| option.value);
        match serde_json::from_str::<T>(&value.unwrap_or_default()) {
            Ok(value) => Ok(Some(value)),
            Err(e) => {
                log::error!(
                    "Error deserializing option {}: {}",
                    self.key_name,
                    e
                );
                Ok(None)
            }
        }
    }

    /// Set the value of this option in the database.
    pub fn set(
        &self,
        conn: &mut SqliteConnection,
        value: &T,
    ) -> diesel::QueryResult<()> {
        let value = serde_json::to_string(value)
            .map_err(|e| DeserializationError(Box::new(e)))?;
        diesel::replace_into(schema::options::table)
            .values(models::ConfigOption {
                name: self.key_name.to_string(),
                value,
            })
            .execute(conn)
            .map(|_| ())
    }

    /// Unset the value of this option in the database.
    pub fn unset(
        &self,
        conn: &mut SqliteConnection,
    ) -> diesel::QueryResult<()> {
        diesel::delete(
            schema::options::table
                .filter(schema::options::name.eq(self.key_name)),
        )
        .execute(conn)
        .map(|_| ())
    }
}
