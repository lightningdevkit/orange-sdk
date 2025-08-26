#[macro_export]
/// This macro generates the `From` implementation for converting a core type to an FFI type.
/// It allows the FFI type to be constructed from the core type directly.
/// Usage: impl_from_core_type!(CoreType, FfiType);
macro_rules! impl_from_core_type {
	($core_type:ident, $ffi_type:ident) => {
		impl From<$core_type> for $ffi_type {
			fn from(core_type: $core_type) -> Self {
				$ffi_type(core_type)
			}
		}
	};
}

#[macro_export]
/// This macro generates the `From` implementation for converting an FFI type to a core type.
/// It allows the core type to be constructed from the FFI type directly.
/// Usage: impl_into_core_type!(FfiType, CoreType);
macro_rules! impl_into_core_type {
	($ffi_type:ident, $core_type:ident) => {
		impl From<$ffi_type> for $core_type {
			fn from(ffi_type: $ffi_type) -> Self {
				ffi_type.0
			}
		}
	};
}
