//! The zero-cost-when-off span guard and the `span!`/`field!` macros that
//! produce it.
//!
//! Under the default build (feature `enabled` off) `Guard` is a zero-sized
//! type and the macros expand to nothing observable: no call site pays for
//! instrumentation it never asked for. Under `enabled`, `Guard` holds the
//! entered `tracing` span and drops it (leaving it) when the guard goes out
//! of scope.

/// Holds an entered span for as long as it is alive. Zero-sized when the
/// `enabled` feature is off.
///
/// The inner field is `pub` only so the [`span!`] macro can construct it
/// from any crate that calls the macro; it is not meant to be constructed
/// or read directly.
#[cfg(feature = "enabled")]
pub struct Guard(pub tracing::span::EnteredSpan);

/// Holds an entered span for as long as it is alive. Zero-sized when the
/// `enabled` feature is off.
#[cfg(not(feature = "enabled"))]
pub struct Guard;

/// Declares a field at span creation whose value is supplied later via
/// [`field!`]. `tracing` only allows recording a field after span creation
/// when the field was declared at creation, so every field a call site fills
/// in after opening its span must be declared with this marker.
#[cfg(feature = "enabled")]
pub use tracing::field::Empty as Pending;

/// Declares a field at span creation whose value is supplied later via
/// [`field!`]. Zero-sized when the `enabled` feature is off.
#[cfg(not(feature = "enabled"))]
pub struct Pending;

/// Opens a span and returns a [`Guard`] that keeps it open until dropped.
///
/// ```ignore
/// let _root = agens_perf::span!("perf.scenario", scenario = name);
/// ```
///
/// With the `enabled` feature off, this expands to a zero-sized `Guard` and
/// none of the arguments are evaluated: a value computed only to fill a
/// field name becomes dead code, which is caught by `-D warnings`.
#[cfg(feature = "enabled")]
#[macro_export]
macro_rules! span {
    ($name:literal $(, $key:ident = $value:expr)* $(,)?) => {
        $crate::Guard($crate::tracing::info_span!($name $(, $key = $value)*).entered())
    };
}

/// Opens a span and returns a [`Guard`] that keeps it open until dropped.
/// Expands to a zero-sized `Guard` with the `enabled` feature off; none of
/// the arguments are evaluated.
#[cfg(not(feature = "enabled"))]
#[macro_export]
macro_rules! span {
    ($name:literal $(, $key:ident = $value:expr)* $(,)?) => {
        $crate::Guard
    };
}

/// Records a value onto a field of the innermost open span, for values only
/// known after the span was opened (e.g. `cache_hit`, decided at return).
/// The field must have been declared at span creation, typically as
/// `key = agens_perf::Pending`.
///
/// With the `enabled` feature off, this expands to `()` and the value is
/// never evaluated.
#[cfg(feature = "enabled")]
#[macro_export]
macro_rules! field {
    ($key:ident = $value:expr) => {
        $crate::tracing::Span::current().record(stringify!($key), $value)
    };
}

/// Records a value onto a field of the innermost open span. Expands to `()`
/// with the `enabled` feature off; the value is never evaluated.
#[cfg(not(feature = "enabled"))]
#[macro_export]
macro_rules! field {
    ($key:ident = $value:expr) => {
        ()
    };
}

#[cfg(all(test, not(feature = "enabled")))]
mod tests {
    use super::Guard;

    #[test]
    fn guard_is_zero_sized() {
        assert_eq!(std::mem::size_of::<Guard>(), 0);
    }
}
