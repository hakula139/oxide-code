//! Names of enum variants that derive `strum`'s `VariantArray` and `IntoStaticStr`.

use anyhow::anyhow;
use strum::VariantArray;

/// Every variant name of `T` in declaration order, comma-separated.
pub(crate) fn names<T>() -> String
where
    T: VariantArray + Copy + Into<&'static str>,
{
    T::VARIANTS
        .iter()
        .map(|&variant| variant.into())
        .collect::<Vec<&str>>()
        .join(", ")
}

/// The parse error for a string that names no variant of `T`.
pub(crate) fn unknown<T>(_: &str) -> anyhow::Error
where
    T: VariantArray + Copy + Into<&'static str>,
{
    anyhow!("expected one of {}", names::<T>())
}
